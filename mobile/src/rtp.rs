//! Minimal RTP video depacketizer (H.264 / H.265 / AV1) → decoder access units.
//!
//! pulsar-core carries video as RTP datagrams (RFC 6184 for H.264, RFC 7798 for
//! H.265, RFC 9043 for AV1), either as a direct UDP flow or wrapped in the encrypted
//! session (`service::media`, tag `TAG_VIDEO`). The desktop client depacketizes in JS
//! (`src/lib/h264.ts`) or lets ffmpeg/mpv do it; the mobile client has neither, so
//! it reassembles NAL units / OBUs here and feeds the resulting access units to an
//! Android `MediaCodec`.
//!
//! H.264/H.265: single NAL, STAP-A (H.264 agg) / AP (H.265 agg), FU-A (H.264 frag)
//! / FU (H.265 frag). DONL is assumed absent (the host's ffmpeg RTP muxer uses
//! `sprop-max-don-diff=0`).
//!
//! AV1: RFC 9043 OBU depacketizer. The 1-byte aggregation header encodes Z (first OBU
//! is a continuation fragment), Y (last OBU is fragmented, continues in next packet),
//! W (3-bit count of OBU elements; 0 means the number is not given / last OBU runs to
//! end), N (new coded video sequence), and reserved bits. Each OBU element is
//! length-prefixed with a variable-length LEB128 integer *except* the last one in a
//! packet when W≠0 (its length is inferred from the remaining bytes). The reassembled
//! output is a sequence of *open_bitstream_unit()* bytes (OBU header preserved, no
//! additional framing) concatenated into one temporal unit, suitable for feeding to an
//! Android `MediaCodec` configured with `"video/av01"`.
//!
//! Access-unit (temporal-unit) boundaries for all codecs: the RTP marker bit signals
//! the last packet of a frame/TU; a defensive timestamp-change flush fires when the
//! marker was lost.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// Annex-B start code prepended to every emitted H.264/H.265 NAL unit.
const START_CODE: [u8; 4] = [0, 0, 0, 1];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
    /// AV1 per RFC 9043. Output is a raw OBU byte-stream temporal unit.
    Av1,
}

/// Return the RTP payload (past the 12-byte header + CSRC list + extension, minus
/// any padding), e.g. the Opus packet for an audio datagram. `None` if the packet
/// is malformed or not RTP v2.
///
/// Strictness matters here: one structurally-invalid "opus packet" reaching Android's
/// `c2.android.opus.decoder` is a FATAL codec error (OPUS_INVALID_PACKET → C2 err 14),
/// so padding bytes must be stripped and non-RTP datagrams rejected.
pub fn rtp_payload(pkt: &[u8]) -> Option<&[u8]> {
    if pkt.len() < 12 {
        return None;
    }
    if pkt[0] >> 6 != 2 {
        return None; // not RTP version 2
    }
    let cc = (pkt[0] & 0x0F) as usize;
    let ext = pkt[0] & 0x10 != 0;
    let mut off = 12 + cc * 4;
    if ext {
        if pkt.len() < off + 4 {
            return None;
        }
        let words = u16::from_be_bytes([pkt[off + 2], pkt[off + 3]]) as usize;
        off += 4 + words * 4;
    }
    // Padding (P bit): the last byte is the pad count, including itself.
    let mut end = pkt.len();
    if pkt[0] & 0x20 != 0 {
        let pad = pkt[end - 1] as usize;
        if pad == 0 || pad > end.saturating_sub(off) {
            return None;
        }
        end -= pad;
    }
    if end <= off {
        return None;
    }
    Some(&pkt[off..end])
}

/// Reassembles RTP video payloads into whole access units.
///
/// - H.264/H.265: outputs an Annex-B byte stream (each NAL prefixed with `[0,0,0,1]`).
/// - AV1: outputs a raw OBU temporal-unit byte stream (concatenated whole OBUs as
///   produced by the encoder, no additional framing). Android `MediaCodec` with
///   `"video/av01"` accepts this directly.
pub struct Depacketizer {
    codec: Codec,
    /// NAL units / OBUs of the access unit being built.
    ///
    /// H.264/H.265: each unit is start-code prefixed.
    /// AV1: raw OBU bytes concatenated; whole OBUs only (fragments reassembled first).
    au: Vec<u8>,
    /// In-progress fragmented NAL (FU-A / FU) without its start code yet.
    /// Also used for the AV1 leading-fragment OBU body while Z or Y is in flight.
    fu: Option<Vec<u8>>,
    last_ts: Option<u32>,
    // ---- AV1 state (RFC 9043) ----
    /// Header byte of the OBU currently being fragment-reassembled (for AV1 Z/Y path).
    /// Stores the original OBU header byte so we can prepend it to the reassembled body.
    av1_frag_obu_hdr: u8,
    /// When true the *next* packet's first OBU element is the continuation of `fu`.
    av1_frag_pending: bool,
    /// A24: a recycled access-unit buffer swapped in on emit (instead of allocating
    /// a fresh empty `Vec`) so the next AU reuses the allocation. Empty until a
    /// caller hands a finished AU back via [`Depacketizer::recycle_au`].
    spare: Vec<u8>,
    /// RTP timestamp of the access unit `push` returned last (see [`Depacketizer::au_ts`]).
    out_ts: u32,
}

impl Depacketizer {
    pub fn new(codec: Codec) -> Self {
        Self {
            codec,
            au: Vec::new(),
            fu: None,
            last_ts: None,
            av1_frag_obu_hdr: 0,
            av1_frag_pending: false,
            spare: Vec::new(),
            out_ts: 0,
        }
    }

    /// RTP timestamp of the access unit the last `push` returned (the frame it belongs to).
    pub fn au_ts(&self) -> u32 {
        self.out_ts
    }

    /// Drop the access unit being assembled (and any fragment in flight): a hole was given up
    /// on, so it is torn. RFI only — the keyframe path marks the next AU torn instead.
    pub fn drop_partial(&mut self) {
        self.au.clear();
        self.fu = None;
        self.av1_frag_pending = false;
    }

    /// Take the finished access unit, swapping in a recycled spare buffer instead of
    /// a fresh empty `Vec` (A24) so the next AU reuses the allocation. Behaves
    /// exactly like `mem::take(&mut self.au)` when no spare has been recycled yet.
    fn take_au(&mut self) -> Vec<u8> {
        std::mem::replace(&mut self.au, std::mem::take(&mut self.spare))
    }

    /// Return a consumed access-unit buffer for reuse as the next AU's backing store
    /// (A24). Cleared and kept as the spare. Optional: callers that don't recycle
    /// simply pay one allocation per AU (the previous behaviour).
    pub fn recycle_au(&mut self, mut au: Vec<u8>) {
        au.clear();
        self.spare = au;
    }

    /// Feed one full RTP datagram (12-byte header included). Returns a complete
    /// Annex-B access unit when a frame boundary is reached (marker bit, or a
    /// timestamp change that flushes the previous frame), else `None`.
    pub fn push(&mut self, pkt: &[u8]) -> Option<Vec<u8>> {
        if pkt.len() < 12 {
            return None;
        }
        if pkt[0] >> 6 != 2 {
            return None; // not RTP v2 — a stray datagram, not our video stream
        }
        let cc = (pkt[0] & 0x0F) as usize;
        let ext = pkt[0] & 0x10 != 0;
        let marker = pkt[1] & 0x80 != 0;
        let ts = u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]);

        // Stale-retransmit guard: a NACK-retransmitted packet arrives ≥1 RTT late —
        // long after its frame was flushed (a 60 fps frame lives ~16 ms). Feeding it
        // anyway bounced `last_ts` backward, so the defensive ts-flush TORE both the
        // in-progress AU and the retransmit itself into partial AUs (observed live:
        // ~2-3x inflated AU counts + heavy visible corruption whenever the link had
        // loss). Drop anything older than the newest timestamp; a huge backward jump
        // (> ~10 s of the 90 kHz clock) is an encoder restart on a fresh random
        // timestamp base → accept and resync instead.
        if let Some(prev) = self.last_ts {
            let d = ts.wrapping_sub(prev) as i32;
            if d < 0 && d > -900_000 {
                return None;
            }
        }

        let mut off = 12 + cc * 4;
        if ext {
            if pkt.len() < off + 4 {
                return None;
            }
            let words = u16::from_be_bytes([pkt[off + 2], pkt[off + 3]]) as usize;
            off += 4 + words * 4;
        }
        // Padding (P bit): the last byte is the pad count, including itself.
        let mut end = pkt.len();
        if pkt[0] & 0x20 != 0 {
            let pad = pkt[end - 1] as usize;
            if pad == 0 || pad > end.saturating_sub(off) {
                return None;
            }
            end -= pad;
        }
        if end <= off {
            return None;
        }
        let payload = &pkt[off..end];

        // Defensive: a new timestamp with an unfinished AU means the previous
        // frame's marker was lost — flush it before starting the new frame.
        let mut out = None;
        if let Some(prev) = self.last_ts.filter(|&prev| prev != ts && !self.au.is_empty()) {
            out = Some(self.take_au()); // A24: reuse a recycled buffer
            self.out_ts = prev;
            self.fu = None;
        }
        self.last_ts = Some(ts);

        match self.codec {
            Codec::H264 => self.process_h264(payload),
            Codec::H265 => self.process_h265(payload),
            Codec::Av1 => self.process_av1(payload),
        }

        if marker && !self.au.is_empty() && out.is_none() {
            out = Some(self.take_au()); // A24: reuse a recycled buffer
            self.out_ts = ts;
        }
        out
    }

    fn emit_nal(&mut self, nal: &[u8]) {
        if nal.is_empty() {
            return;
        }
        self.au.extend_from_slice(&START_CODE);
        self.au.extend_from_slice(nal);
    }

    fn process_h264(&mut self, payload: &[u8]) {
        if payload.is_empty() {
            return;
        }
        match payload[0] & 0x1F {
            1..=23 => self.emit_nal(payload), // single NAL unit
            24 => {
                // STAP-A: [NAL hdr][ (size:u16)(NAL) ]*
                let mut i = 1;
                while i + 2 <= payload.len() {
                    let size = u16::from_be_bytes([payload[i], payload[i + 1]]) as usize;
                    i += 2;
                    if i + size > payload.len() {
                        break;
                    }
                    let nal = payload[i..i + size].to_vec();
                    self.emit_nal(&nal);
                    i += size;
                }
            }
            28 => {
                // FU-A: [FU indicator][FU header][fragment]
                if payload.len() < 2 {
                    return;
                }
                let (ind, hdr) = (payload[0], payload[1]);
                let frag = &payload[2..];
                if hdr & 0x80 != 0 {
                    // start: rebuild NAL header = F|NRI from indicator + type from FU header
                    let mut nal = Vec::with_capacity(1 + frag.len());
                    nal.push((ind & 0xE0) | (hdr & 0x1F));
                    nal.extend_from_slice(frag);
                    self.fu = Some(nal);
                } else if let Some(buf) = self.fu.as_mut() {
                    buf.extend_from_slice(frag);
                }
                if hdr & 0x40 != 0 {
                    if let Some(nal) = self.fu.take() {
                        self.emit_nal(&nal);
                    }
                }
            }
            _ => {} // STAP-B / MTAP / FU-B: unused by the host's ffmpeg muxer
        }
    }

    // -------------------------------------------------------------------------
    // AV1 — RFC 9043
    // -------------------------------------------------------------------------
    //
    // RTP payload layout (§4):
    //
    //   +-+-+-+-+-+-+-+-+
    //   |Z|Y|W W W|N|V|0|   ← 1-byte aggregation header
    //   +-+-+-+-+-+-+-+-+
    //   followed by one or more OBU elements, each:
    //     [ leb128_length ] [ OBU bytes ]
    //   …except the very last element in the packet when W ≠ 0, whose length is
    //   omitted (inferred from remaining bytes).
    //
    //   Z = 1 → the first OBU element is a continuation fragment of the OBU
    //            that ended the previous packet (i.e. reassemble with `fu`).
    //   Y = 1 → the last OBU element is fragmented; its tail arrives in the
    //            next packet as a Z=1 element.
    //   W[2:0] → number of OBU elements in this packet (0 = not signalled;
    //             treat each element as length-prefixed until end of payload).
    //   N = 1 → new coded video sequence starts here (random access point /
    //            key-frame; carries the sequence header OBU).
    //   V = 1 → reserved (was previously "video layer ID present"); IGNORE.
    //   bit 0 → always 0 (reserved).
    //
    // The output for Android MediaCodec ("video/av01") is a temporal unit:
    // consecutive whole OBU bytes, no additional framing. We therefore append
    // each reassembled OBU to `self.au` and emit on the RTP marker bit.

    fn process_av1(&mut self, payload: &[u8]) {
        if payload.is_empty() {
            return;
        }
        let agg_hdr = payload[0];
        let z = agg_hdr & 0x80 != 0; // first element is a fragment continuation
        let y = agg_hdr & 0x40 != 0; // last element is fragmented (continues next pkt)
        let w = ((agg_hdr >> 3) & 0x07) as usize; // OBU element count (0 = unknown)
        // N (new sequence) and V (reserved) bits are informational for us — no action needed.

        let mut pos = 1usize; // cursor into payload (past the aggregation header)
        let mut elem_idx = 0usize; // how many OBU elements we have processed

        while pos < payload.len() {
            // Determine whether this element's length is length-prefixed.
            // The last element in the packet is NOT length-prefixed when W ≠ 0.
            let is_last_signalled = w != 0 && elem_idx == w.saturating_sub(1);

            let obu_len = if is_last_signalled {
                // Last element: runs to end of payload.
                payload.len() - pos
            } else {
                // Read LEB128 length.
                match read_leb128(&payload[pos..]) {
                    Some((len, consumed)) => {
                        pos += consumed;
                        len
                    }
                    None => return, // malformed
                }
            };

            if pos + obu_len > payload.len() {
                return; // malformed / truncated
            }

            let obu_bytes = &payload[pos..pos + obu_len];
            pos += obu_len;

            let is_first_elem = elem_idx == 0;
            // Determine if this element is the last one in the packet.
            let is_last_elem = if w != 0 {
                elem_idx == w.saturating_sub(1)
            } else {
                pos >= payload.len()
            };

            if is_first_elem && z {
                // Continuation fragment: append to the in-progress OBU body in `fu`.
                if let Some(buf) = self.fu.as_mut() {
                    buf.extend_from_slice(obu_bytes);
                }
                // If this is also the last element and Y=0, the OBU is now complete.
                if is_last_elem && !y {
                    if let Some(body) = self.fu.take() {
                        // Prepend the saved OBU header byte.
                        self.au.push(self.av1_frag_obu_hdr);
                        self.au.extend_from_slice(&body);
                    }
                    self.av1_frag_pending = false;
                }
                // If Z=1 and Y=1 and this is the only element, the OBU fragment
                // runs across both the Z-continuation and Y-fragmentation: keep
                // accumulating into `fu` (av1_frag_pending stays true).
            } else if is_last_elem && y {
                // This OBU element is fragmented; its tail arrives next packet.
                // Save the OBU header byte and start a new fragment buffer.
                if obu_bytes.is_empty() {
                    elem_idx += 1;
                    continue;
                }
                self.av1_frag_obu_hdr = obu_bytes[0];
                let body = obu_bytes[1..].to_vec();
                self.fu = Some(body);
                self.av1_frag_pending = true;
            } else {
                // Whole OBU: append directly.
                // Any previously pending fragment is implicitly abandoned (shouldn't
                // happen in a well-behaved stream; discard defensively).
                if self.av1_frag_pending {
                    self.fu = None;
                    self.av1_frag_pending = false;
                }
                self.au.extend_from_slice(obu_bytes);
            }

            elem_idx += 1;
        }
    }

    fn process_h265(&mut self, payload: &[u8]) {
        if payload.len() < 2 {
            return;
        }
        match (payload[0] >> 1) & 0x3F {
            0..=47 => self.emit_nal(payload), // single NAL unit
            48 => {
                // AP (aggregation), DONL absent: [2-byte NAL hdr][ (size:u16)(NAL) ]*
                let mut i = 2;
                while i + 2 <= payload.len() {
                    let size = u16::from_be_bytes([payload[i], payload[i + 1]]) as usize;
                    i += 2;
                    if i + size > payload.len() {
                        break;
                    }
                    let nal = payload[i..i + size].to_vec();
                    self.emit_nal(&nal);
                    i += size;
                }
            }
            49 => {
                // FU, DONL absent: [2-byte NAL hdr][FU header][fragment]
                if payload.len() < 3 {
                    return;
                }
                let (b0, b1, hdr) = (payload[0], payload[1], payload[2]);
                let frag = &payload[3..];
                if hdr & 0x80 != 0 {
                    // start: rebuild 2-byte NAL header, replacing the 6-bit type
                    let mut nal = Vec::with_capacity(2 + frag.len());
                    nal.push((b0 & 0x81) | ((hdr & 0x3F) << 1));
                    nal.push(b1);
                    nal.extend_from_slice(frag);
                    self.fu = Some(nal);
                } else if let Some(buf) = self.fu.as_mut() {
                    buf.extend_from_slice(frag);
                }
                if hdr & 0x40 != 0 {
                    if let Some(nal) = self.fu.take() {
                        self.emit_nal(&nal);
                    }
                }
            }
            _ => {}
        }
    }
}

// -------------------------------------------------------------------------
// LEB128 unsigned integer decoder (used by the AV1 OBU element length field,
// RFC 9043 §3.2 / AV1 spec §4.10.5).
//
// Returns `(value, bytes_consumed)` or `None` if the input is empty or the
// integer requires more than 8 bytes (64-bit overflow guard).
// -------------------------------------------------------------------------
fn read_leb128(buf: &[u8]) -> Option<(usize, usize)> {
    let mut value: u64 = 0;
    let mut shift = 0u32;
    for (i, &byte) in buf.iter().enumerate() {
        if shift >= 64 {
            return None; // overflow
        }
        value |= ((byte & 0x7F) as u64) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            // No more bytes; check the result fits in usize.
            return usize::try_from(value).ok().map(|v| (v, i + 1));
        }
    }
    None // truncated
}

// =========================================================================
// A7 — RTP reorder / jitter buffer (THE spike fix)
// =========================================================================
//
// Sits between `service::media::parse` and the `Depacketizer` in the client read
// loop. A single late or reordered datagram otherwise trips the depacketizer's
// gap handling and corrupts the in-flight access unit, producing a visible decode
// spike. This buffer holds out-of-order packets for a brief, RTT-bounded window
// and releases them in sequence order. In-order packets (the steady-state common
// case) fall straight through with no added latency.
//
// It only ORDERS: the NACKs (reorder / FEC waits, one re-NACK, the write-off) and the
// loss the ABR runs on come from the shared repair loop (`pulsar_core::adapt::repair::
// RepairTracker`, the desktop client's), which also sizes this buffer's wait
// (`set_max_delay_ms` = its repair window plus one sweep — the desktop renderer's
// `maxdelay`), so both give up on a hole together.
//
// Packets are keyed by a 64-bit *extended* sequence number reconstructed from the
// 16-bit RTP seq (so the map orders correctly across the 16-bit wrap). On a stream
// discontinuity (encoder restart with a fresh RTP base — every codec / monitor
// switch rebuilds it) the buffer resyncs to the new base instead of stalling.

/// Max buffered packets before we force a flush. A MEMORY guard only: the buffer is bounded
/// by time (`max_delay`, at most `MAX_DELAY_CAP_MS`), and a hole must be able to wait out its
/// repair window at any rate this client streams. The old 128 flushed past a hole after
/// 25-40 ms at 30-50 Mbit (3-5k packets/s), often before its delayed NACK had even left, so
/// no hole could be repaired at those rates. 4096 holds 250 ms up to ~16k packets/s (the
/// desktop renderer holds 512); ~6 MB at worst, only while a hole is held.
const REORDER_CAPACITY: usize = 4096;

/// Free-list bound for recycled packet buffers (separate from the hold bound above: the
/// steady state needs only a handful).
const POOL_CAPACITY: usize = 128;

/// Seq distance beyond which a jump is a stream restart (fresh RTP base) rather
/// than ordinary loss/reorder — resync to it instead of NACK-flooding / stalling.
/// Measured outside the window the buffer covers: behind the release cursor, or ahead
/// of the newest packet held (packets held behind a hole can be far more than this
/// ahead of the cursor).
const REORDER_RESYNC_DISTANCE: i32 = 256;

/// The longest a hole may hold the packets behind it (ms): the repair loop's bound
/// (`pulsar_core::adapt::repair::RENDERER_WAIT_MAX_MS`, the desktop renderer's cap).
const MAX_DELAY_CAP_MS: f32 = pulsar_core::adapt::repair::RENDERER_WAIT_MAX_MS;

// ── Reference-frame invalidation (RFI) ─────────────────────────────────────────────────
//
// Ported from the desktop renderer (`pulsar-desktop` crates/pulsar-render/src/stream/rtp.rs),
// constants and semantics unchanged. When the host advertises `FEAT_RFI` and streams with a
// deep reference buffer for us (`StreamReq::rfi`), a hole is reported to it early —
// `RFI_REPORT_AFTER` after the first packet behind it, longer on a path that reorders — while
// NACK repair still runs, so the host can still reach the lost frame in its reference buffer:
// it invalidates it and tags its next frame (predicted from an older, intact one) with a
// Pulsar SEI. If the repair fails, the client drops access units until that tag (or a
// keyframe) instead of waiting for an IDR; no recovery within `rfi_timeout` → the keyframe
// request after all. A repaired hole costs nothing but a slightly less efficient frame.

/// How long a hole must persist before it is reported, at least (plain reordering heals
/// faster)…
const RFI_REPORT_AFTER: Duration = Duration::from_millis(3);
/// …and while the path reorders, `REORDER_SLACK` × the recent reorder lateness, at most
/// `RFI_REPORT_MAX` (a hole given up on is reported at once). The desktop measured a fixed
/// 3 ms reporting every late packet of a reordering path as lost (each answered with a
/// recovery frame, release-at-tag then skipped packets still in flight: 11 fps).
const REORDER_SLACK: f32 = 1.25;
const RFI_REPORT_MAX: Duration = Duration::from_millis(100);
/// How fast the reorder lateness peak decays (halves every this long without a new sample).
const LATE_HALF_LIFE: Duration = Duration::from_secs(2);
/// Waiting longer than this for a recovery frame → ask for a keyframe (`rfi_timeout`: at least
/// this, more on a slow path). The wait starts only when the repair window ran out, by which
/// time a recovery frame (sent ~1 RTT after the report) has normally arrived already. Every
/// further hole seen while waiting restarts it (that hole was reported too).
pub const RFI_TIMEOUT: Duration = Duration::from_millis(150);
/// Upper bound on one recovery wait, however many holes keep restarting `RFI_TIMEOUT`.
pub const RFI_GIVE_UP: Duration = Duration::from_millis(1000);
/// The host's recovery-tag SEI UUID.
const RFI_UUID: [u8; 16] = pulsar_core::service::media::RFI_RECOVERY_UUID;

/// The recovery wait for the current RTT: `RFI_TIMEOUT`, or 1.5 RTT + 50 ms when that is
/// longer (a report can only be answered after a round trip), never past `RFI_GIVE_UP`.
pub fn rfi_timeout(rtt: Option<Duration>) -> Duration {
    let by_rtt = rtt.map_or(Duration::ZERO, |r| r * 3 / 2 + Duration::from_millis(50));
    RFI_TIMEOUT.max(by_rtt).min(RFI_GIVE_UP)
}

/// The RTP packet is the host's RFI recovery tag: a single-NAL packet holding the
/// user-data-unregistered SEI with `RFI_UUID` (H.264 type 6 or HEVC prefix SEI 39). The host
/// sends it as a packet of its own, the first of the recovery frame.
pub fn is_recovery_tag_packet(p: &[u8]) -> bool {
    let Some(pl) = rtp_payload(p) else { return false };
    let at = |h: usize| pl.len() >= h + 18 && pl[h] == 5 && pl[h + 1] == 16 && pl[h + 2..h + 18] == RFI_UUID;
    (!pl.is_empty() && pl[0] & 0x1f == 6 && at(1)) || (pl.len() > 2 && (pl[0] >> 1) & 0x3f == 39 && at(2))
}

/// The Annex-B access unit carries the host's RFI recovery tag (a user-data-unregistered SEI).
pub fn au_has_recovery_tag(codec: Codec, au: &[u8]) -> bool {
    let (hevc, hdr) = match codec {
        Codec::H264 => (false, 1),
        Codec::H265 => (true, 2),
        Codec::Av1 => return false,
    };
    annexb_any_nal_at(au, |n| {
        let sei = if hevc { n.len() > 2 && (n[0] >> 1) & 0x3f == 39 } else { !n.is_empty() && n[0] & 0x1f == 6 };
        sei && n.len() >= hdr + 18 && n[hdr] == 5 && n[hdr + 1] == 16 && n[hdr + 2..hdr + 18] == RFI_UUID
    })
}

/// RFI resume gate for the client's decode path (the desktop depacketizer's `awaiting_idr` /
/// `awaiting_recovery` / `rfi_fallback` logic): which access units may go to the decoder after
/// a hole the reorder buffer gave up on. Used only while the stream runs with RFI; the plain
/// wait-for-keyframe gate in the client covers the rest. Pure (time is passed in).
#[derive(Debug, Default)]
pub struct RfiGate {
    /// Access units are dropped until a whole keyframe (or, after a timed-out recovery wait,
    /// a whole recovery frame — `fallback`).
    waiting_idr: bool,
    /// Access units are dropped until the host's recovery frame or a keyframe: (first hole of
    /// this wait, latest hole).
    awaiting_recovery: Option<(Instant, Instant)>,
    /// The keyframe wait came from a recovery wait that timed out — a whole recovery frame
    /// still ends it: holes keep being reported, and the host invalidates every frame
    /// predicted from a lost one, so its recovery frame references one this decoder holds. At
    /// high loss a small recovery frame arrives whole long before a large keyframe does.
    fallback: bool,
    /// RTP timestamp of the access unit the last hole ran into: never decoded (torn).
    gap_ts: Option<u32>,
}

impl RfiGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// The reorder buffer gave up on a hole; `post` is the first packet released after it.
    /// The access unit it belongs to is torn — unless it is the recovery tag itself (a packet
    /// of its own, the first of its frame: the common case at high loss). Then wait for the
    /// recovery frame; a further hole while waiting restarts the timeout.
    pub fn on_hole(&mut self, post: &[u8], now: Instant) {
        self.gap_ts = if is_recovery_tag_packet(post) { None } else { rtp_ts(post) };
        if !self.waiting_idr {
            self.awaiting_recovery = Some(match self.awaiting_recovery {
                Some((first, _)) => (first, now),
                None => (now, now),
            });
        }
    }

    /// The stream restarted on a fresh RTP base (`post` = its first packet): wait for its
    /// keyframe; no recovery frame can bridge an encoder restart.
    pub fn on_resync(&mut self, post: &[u8]) {
        self.awaiting_recovery = None;
        self.fallback = false;
        self.waiting_idr = true;
        self.gap_ts = rtp_ts(post);
    }

    /// Access units are being dropped until a keyframe (the client asks the host for one).
    pub fn wants_keyframe(&self) -> bool {
        self.waiting_idr
    }

    /// A recovery wait that ran out turns into a keyframe wait. True when it just did.
    pub fn poll(&mut self, now: Instant, rtt: Option<Duration>) -> bool {
        let Some((first, last)) = self.awaiting_recovery else { return false };
        if now.saturating_duration_since(last) >= rfi_timeout(rtt) || now.saturating_duration_since(first) >= RFI_GIVE_UP {
            self.awaiting_recovery = None;
            self.waiting_idr = true;
            self.fallback = true;
            return true;
        }
        false
    }

    /// May this access unit (RTP timestamp `ts`) go to the decoder? A torn one never does
    /// (not even a keyframe); while waiting, only a whole keyframe or recovery frame ends the
    /// wait.
    pub fn admit(&mut self, codec: Codec, au: &[u8], ts: u32, now: Instant, rtt: Option<Duration>) -> bool {
        let whole = self.gap_ts != Some(ts);
        let key = whole && au_is_keyframe(codec, au);
        let tag = whole && au_has_recovery_tag(codec, au);
        if self.waiting_idr {
            if !key && !(self.fallback && tag) {
                return false;
            }
            self.waiting_idr = false;
            self.fallback = false;
        }
        if self.awaiting_recovery.is_some() {
            if key || tag {
                self.awaiting_recovery = None;
            } else {
                self.poll(now, rtt);
                return false;
            }
        }
        if !whole {
            // A torn AU outside any wait (cannot happen after `on_hole`): its references are
            // gone, so wait for a keyframe.
            self.waiting_idr = true;
            return false;
        }
        true
    }
}

/// RTP timestamp of a datagram.
fn rtp_ts(p: &[u8]) -> Option<u32> {
    (p.len() >= 12).then(|| u32::from_be_bytes([p[4], p[5], p[6], p[7]]))
}

pub struct ReorderBuffer {
    /// Out-of-order packets awaiting release, keyed by extended seq, each with its arrival
    /// time (RFI report delay). Owned copies (the source recv datagram is reused by the
    /// batch). Recycled via `recycle`.
    buf: BTreeMap<u64, (Instant, Vec<u8>)>,
    /// Extended seq of the next packet to release in order (`None` until primed).
    next: Option<u64>,
    /// Extended seq of the newest packet seen (restart detection, see
    /// `REORDER_RESYNC_DISTANCE`).
    highest: u64,
    /// Max time the head-of-line packet may wait for a missing predecessor before
    /// we flush past the hole (a "declared gap"). See `set_max_delay_ms`.
    max_delay: Duration,
    /// When the current head-of-line block started waiting.
    blocked_since: Option<Instant>,
    /// Set by `pop_ready` when it skipped past a missing packet; read+cleared by the
    /// caller (`take_gap_declared`) to engage the awaiting-IDR gate + keyframe NACK.
    gap_declared: bool,
    /// The declared gap was a stream restart (fresh RTP base), not a hole (`take_resynced`).
    resynced: bool,
    /// Window counters for the ABR controller's *true* loss (declared gaps only).
    win_recv: u32,
    win_lost: u32,
    /// Free list of packet buffers so the hot path stays allocation-free.
    pool: Vec<Vec<u8>>,
    /// RFI: holes are reported to the host (`set_rfi`).
    rfi: bool,
    /// RFI: hole starts already reported; pruned once `next` passes them.
    reported: Vec<u64>,
    /// RFI: held packets that are the host's recovery tag (`is_recovery_tag_packet`).
    tags: BTreeSet<u64>,
    /// RFI: the lost range waiting to be sent to the host (merged since the last take).
    rfi_req: Option<(u16, u16)>,
    /// How late packets arrive behind their successors on this path (ms, a peak that halves
    /// every `LATE_HALF_LIFE`) and when it was last raised — sizes the RFI report delay.
    late_peak: f32,
    late_at: Option<Instant>,
}

impl Default for ReorderBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl ReorderBuffer {
    pub fn new() -> Self {
        Self {
            buf: BTreeMap::new(),
            next: None,
            highest: 0,
            // No RTT yet: the repair loop's upper bound (it is sized from the first Pong on).
            max_delay: Duration::from_secs_f32(MAX_DELAY_CAP_MS / 1000.0),
            blocked_since: None,
            gap_declared: false,
            resynced: false,
            win_recv: 0,
            win_lost: 0,
            pool: Vec::new(),
            rfi: false,
            reported: Vec::new(),
            tags: BTreeSet::new(),
            rfi_req: None,
            late_peak: 0.0,
            late_at: None,
        }
    }

    /// Drop every held packet (recycling the buffers).
    fn drop_held(&mut self) {
        let drained: Vec<Vec<u8>> = self.buf.values_mut().map(|(_, v)| std::mem::take(v)).collect();
        self.buf.clear();
        for v in drained {
            self.recycle(v);
        }
        self.reported.clear();
        self.tags.clear();
    }

    /// Drop all buffered state — called on restream, where the host restarts its
    /// RTP sequence base (and the depacketizer is rebuilt).
    pub fn reset(&mut self) {
        self.drop_held();
        self.next = None;
        self.blocked_since = None;
        self.gap_declared = false;
        self.resynced = false;
        self.win_recv = 0;
        self.win_lost = 0;
        self.rfi_req = None;
    }

    /// RFI on / off for this stream (see `RfiGate`). Off: nothing is reported or released early.
    pub fn set_rfi(&mut self, on: bool) {
        self.rfi = on;
        if !on {
            self.reported.clear();
            self.tags.clear();
            self.rfi_req = None;
        }
    }

    /// The lost range to report to the host (`DataMsg::MediaRfi`), if any, merged since the
    /// last call.
    pub fn take_rfi_request(&mut self) -> Option<(u16, u16)> {
        self.rfi_req.take()
    }

    fn report_lost(&mut self, first: u64, last: u64) {
        self.reported.push(first);
        let (first, last) = (first as u16, last as u16);
        self.rfi_req = Some(match self.rfi_req {
            Some((f, _)) => (f, last),
            None => (first, last),
        });
    }

    /// The reorder lateness peak at `now` (ms, decayed).
    fn late_ms(&self, now: Instant) -> f32 {
        let Some(at) = self.late_at else { return 0.0 };
        let half_lives = now.saturating_duration_since(at).as_secs_f32() / LATE_HALF_LIFE.as_secs_f32();
        self.late_peak * 0.5f32.powf(half_lives)
    }

    /// A packet filled a hole `late` after a later packet had arrived.
    fn note_late(&mut self, now: Instant, late: Duration) {
        let ms = late.as_secs_f32() * 1000.0;
        if ms >= self.late_ms(now) {
            self.late_peak = ms;
            self.late_at = Some(now);
        }
    }

    /// How long a hole waits before it is reported for RFI (see `REORDER_SLACK`).
    fn report_after(&self, now: Instant) -> Duration {
        Duration::from_secs_f32(self.late_ms(now) * REORDER_SLACK / 1000.0).clamp(RFI_REPORT_AFTER, RFI_REPORT_MAX)
    }

    /// RFI: report every hole in front of a held packet that has persisted `report_after`,
    /// once each (the early hole report — NACK repair keeps running meanwhile). Call once per
    /// drain, before `pop_ready`.
    pub fn poll_rfi(&mut self, now: Instant) {
        if !self.rfi {
            return;
        }
        let Some(next) = self.next else { return };
        self.reported.retain(|&s| s >= next);
        // Fast path: no hole at all (the held run is contiguous from `next`).
        match (self.buf.keys().next(), self.buf.keys().next_back()) {
            (Some(&lo), Some(&hi)) if lo == next && hi - lo + 1 == self.buf.len() as u64 => return,
            (None, _) => return,
            _ => {}
        }
        let wait = self.report_after(now);
        let mut expect = next;
        let mut found: Vec<(u64, u64)> = Vec::new();
        for (&s, (t, _)) in &self.buf {
            if s != expect && now.saturating_duration_since(*t) >= wait && !self.reported.contains(&expect) {
                found.push((expect, s - 1));
            }
            expect = s + 1;
        }
        for (f, l) in found {
            self.report_lost(f, l);
        }
    }

    /// How long a hole may hold the packets behind it (ms): the shared repair loop's
    /// `RepairTracker::renderer_wait_ms` — its RTT-derived repair window plus one sweep
    /// (60-250 ms), the desktop renderer's `maxdelay`. The old `1.5 × RTT + 100 ms`
    /// (100-500 ms) froze every frame behind a hole for half a second on a jittery mobile
    /// link. Capped at `MAX_DELAY_CAP_MS`, never under 1 ms.
    pub fn set_max_delay_ms(&mut self, ms: f32) {
        let ms = if ms.is_finite() { ms.clamp(1.0, MAX_DELAY_CAP_MS) } else { MAX_DELAY_CAP_MS };
        self.max_delay = Duration::from_secs_f32(ms / 1000.0);
    }

    /// The current hold window (see `set_max_delay_ms`).
    #[cfg(test)]
    pub fn max_delay(&self) -> Duration {
        self.max_delay
    }

    fn take_buf(&mut self, data: &[u8]) -> Vec<u8> {
        let mut v = self.pool.pop().unwrap_or_default();
        v.clear();
        v.extend_from_slice(data);
        v
    }

    /// Return a released buffer to the free list for reuse.
    pub fn recycle(&mut self, buf: Vec<u8>) {
        if self.pool.len() < POOL_CAPACITY {
            self.pool.push(buf);
        }
    }

    /// Insert one video RTP packet (an owned copy is taken), arriving now.
    #[cfg(test)]
    pub fn push(&mut self, seq: u16, pkt: &[u8]) {
        self.push_at(seq, pkt, Instant::now());
    }

    /// Insert one video RTP packet (an owned copy is taken) that arrived at `now`.
    pub fn push_at(&mut self, seq: u16, pkt: &[u8], now: Instant) {
        self.win_recv = self.win_recv.saturating_add(1);

        let next = match self.next {
            Some(n) => n,
            None => {
                // First packet primes the release cursor.
                let ext = seq as u64;
                self.next = Some(ext);
                self.highest = ext;
                self.hold(ext, pkt, now);
                return;
            }
        };

        // Reconstruct the extended seq relative to the release cursor (wrap-aware:
        // the signed 16-bit delta picks the nearest extended value).
        let delta = seq.wrapping_sub(next as u16) as i16 as i32;
        let ext = (next as i64 + delta as i64) as u64;
        let ahead_of_newest = seq.wrapping_sub(self.highest as u16) as i16 as i32;

        // A jump larger than any plausible reorder/loss burst = the encoder restarted
        // with a fresh RTP base. Resync to it (re-anchoring the decoder) instead of
        // treating the whole distance as loss and NACK-flooding / stalling forever.
        // Far behind the cursor, or far ahead of the newest packet: everything between
        // (a retransmit of a held hole, packets queued behind it) is this stream.
        if delta < -REORDER_RESYNC_DISTANCE || ahead_of_newest > REORDER_RESYNC_DISTANCE {
            self.drop_held();
            self.next = Some(ext);
            self.highest = ext;
            self.blocked_since = None;
            self.gap_declared = true; // ask the decoder to re-anchor on the new stream
            self.resynced = true;
            self.hold(ext, pkt, now);
            return;
        }

        // Behind the cursor (already released or given up on) or a duplicate → drop.
        if ext < next || self.buf.contains_key(&ext) {
            return;
        }
        if ext < self.highest {
            // A hole filled: how long after the first later packet did it come?
            if let Some(t) = self.buf.range(ext + 1..).map(|(_, (t, _))| *t).min() {
                self.note_late(now, now.saturating_duration_since(t));
            }
        }
        self.highest = self.highest.max(ext);

        self.hold(ext, pkt, now);
    }

    fn hold(&mut self, ext: u64, pkt: &[u8], now: Instant) {
        if self.rfi && is_recovery_tag_packet(pkt) {
            self.tags.insert(ext);
        }
        let b = self.take_buf(pkt);
        self.buf.insert(ext, (now, b));
    }

    /// Pop the next packet to feed the depacketizer, in sequence order. Returns
    /// `None` while the head-of-line packet is missing and still within its wait
    /// window. On overflow or timeout it skips the hole (declaring a gap — see
    /// `take_gap_declared`) and resumes releasing in order. RFI: a hole already reported
    /// to the host is skipped at once when its recovery frame is held behind it — every
    /// frame from the hole up to it was predicted from the lost one, and holding them for
    /// the whole repair window would release them as one burst behind the recovery frame.
    pub fn pop_ready(&mut self, now: Instant) -> Option<Vec<u8>> {
        loop {
            let next = self.next?;
            let &first = self.buf.keys().next()?;

            if first < next {
                // Stale leftover (push normally drops these) — discard defensively.
                if let Some((_, v)) = self.buf.remove(&first) {
                    self.recycle(v);
                }
                continue;
            }
            if first == next {
                let (_, v) = self.buf.remove(&first).expect("key just observed");
                self.tags.remove(&first);
                self.next = Some(next.wrapping_add(1));
                self.blocked_since = None;
                return Some(v);
            }

            // first > next → a hole at `next`. Hold until max_delay / capacity.
            let overflow = self.buf.len() > REORDER_CAPACITY;
            // RFI: only past a hole reported as lost — a fresh one in front of the tag is most
            // likely a packet still on its way (the tag answers an older report).
            let recovered = self.rfi && self.reported.contains(&next) && self.tags.range(next..).next().is_some();
            let timed_out = match self.blocked_since {
                Some(t) => now.saturating_duration_since(t) >= self.max_delay,
                None => {
                    self.blocked_since = Some(now);
                    false
                }
            };
            if !(overflow || timed_out || recovered) {
                return None;
            }

            // A hole given up on before it was reported (the buffer filled up, a short wait)
            // is reported now.
            if self.rfi && !self.reported.contains(&next) {
                self.report_lost(next, first - 1);
            }
            // Declare a gap: count the skipped seqs as true loss and jump the cursor
            // to the next available packet (released in order on the next iteration).
            self.win_lost = self.win_lost.saturating_add((first - next) as u32);
            self.next = Some(first);
            self.blocked_since = None;
            self.gap_declared = true;
        }
    }

    /// True (and cleared) if `pop_ready` skipped past a missing packet since the last
    /// call — the caller engages the awaiting-IDR gate + sends a keyframe NACK.
    pub fn take_gap_declared(&mut self) -> bool {
        std::mem::replace(&mut self.gap_declared, false)
    }

    /// True (and cleared) if the last declared gap was a stream restart (fresh RTP base).
    pub fn take_resynced(&mut self) -> bool {
        std::mem::replace(&mut self.resynced, false)
    }

    /// Window `(received, lost)` packet counts, resetting the window. `lost` counts only
    /// declared gaps (holes flushed past) — NOT reordering. Diagnostics: the ABR runs on
    /// the repair loop's window (`RepairTracker::take_window`).
    pub fn take_loss_window(&mut self) -> (u32, u32) {
        let out = (self.win_recv, self.win_lost);
        self.win_recv = 0;
        self.win_lost = 0;
        out
    }
}

/// Whether a depacketized access unit is a keyframe / random-access point. Used by
/// the client's awaiting-IDR gate (A7) to know when decoding may safely resume after
/// a declared packet-loss gap. Conservative: anything it cannot classify is treated
/// as a keyframe so the gate can never wedge the stream permanently.
pub fn au_is_keyframe(codec: Codec, au: &[u8]) -> bool {
    match codec {
        Codec::H264 => annexb_any_nal(au, |b| {
            let t = b & 0x1F;
            t == 5 || t == 7 // IDR slice or SPS
        }),
        Codec::H265 => annexb_any_nal(au, |b| {
            let t = (b >> 1) & 0x3F;
            (16..=23).contains(&t) || t == 32 || t == 33 // IRAP (BLA/IDR/CRA) or VPS/SPS
        }),
        Codec::Av1 => av1_tu_has_seq_header(au),
    }
}

/// Scan an Annex-B byte stream; return true if any NAL's first byte satisfies
/// `pred`. Matches the 3-byte start code `00 00 01` (the 4-byte `00 00 00 01`
/// contains it), which is how `Depacketizer::emit_nal` frames every NAL.
fn annexb_any_nal(au: &[u8], pred: impl Fn(u8) -> bool) -> bool {
    let mut i = 0usize;
    while i + 3 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            let nal0 = i + 3;
            if pred(au[nal0]) {
                return true;
            }
            i = nal0;
        } else {
            i += 1;
        }
    }
    false
}

/// Like `annexb_any_nal`, but `pred` sees each NAL's bytes (up to the next start code).
fn annexb_any_nal_at(au: &[u8], pred: impl Fn(&[u8]) -> bool) -> bool {
    let mut starts = Vec::new();
    let mut i = 0usize;
    while i + 3 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts.iter().enumerate().any(|(k, &s)| {
        // The NAL ends where the next start code begins (its 3- or 4-byte form).
        let end = starts.get(k + 1).map_or(au.len(), |&n| {
            let e = n - 3;
            if e > s && au[e - 1] == 0 {
                e - 1
            } else {
                e
            }
        });
        pred(&au[s..end.max(s)])
    })
}

/// AV1: true if the temporal unit contains a sequence-header OBU (type 1), which the
/// encoder emits at every random-access point (keyframe). Walks the OBU chain using
/// its `obu_has_size_field` LEB128 sizes; on anything unparseable it fails safe
/// (returns true) so the awaiting-IDR gate never wedges.
fn av1_tu_has_seq_header(au: &[u8]) -> bool {
    let mut i = 0usize;
    while i < au.len() {
        let hdr = au[i];
        let obu_type = (hdr >> 3) & 0x0F;
        if obu_type == 1 {
            return true; // OBU_SEQUENCE_HEADER
        }
        let ext = hdr & 0x04 != 0;
        let has_size = hdr & 0x02 != 0;
        let mut p = i + 1;
        if ext {
            p += 1;
        }
        if !has_size {
            return true; // cannot advance without a size field — fail safe
        }
        match read_leb128(au.get(p..).unwrap_or(&[])) {
            Some((len, consumed)) => {
                p = p.saturating_add(consumed).saturating_add(len);
            }
            None => return true, // truncated / unparseable — fail safe
        }
        if p <= i {
            return true; // no forward progress — fail safe
        }
        i = p;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an RTP datagram (no CSRC, no extension) around a payload.
    fn rtp(marker: bool, ts: u32, seq: u16, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0x80, if marker { 0x80 | 96 } else { 96 }];
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(&ts.to_be_bytes());
        p.extend_from_slice(&[0, 0, 0, 0]); // SSRC
        p.extend_from_slice(payload);
        p
    }

    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for n in nals {
            v.extend_from_slice(&START_CODE);
            v.extend_from_slice(n);
        }
        v
    }

    #[test]
    fn rtp_payload_strips_padding() {
        let mut p = rtp(true, 1, 1, &[0xAA, 0xBB, 0xCC]);
        // Set the P bit and append 2 pad bytes (last byte = pad count).
        p[0] |= 0x20;
        p.extend_from_slice(&[0x00, 0x02]);
        assert_eq!(rtp_payload(&p), Some(&[0xAA, 0xBB, 0xCC][..]));
    }

    #[test]
    fn rtp_payload_rejects_bad_padding_and_version() {
        // Pad count larger than the payload → malformed.
        let mut p = rtp(true, 1, 1, &[0xAA]);
        p[0] |= 0x20;
        p.push(200);
        assert_eq!(rtp_payload(&p), None);
        // Pad count of zero with the P bit set → malformed.
        let mut z = rtp(true, 1, 1, &[0xAA]);
        z[0] |= 0x20;
        z.push(0);
        assert_eq!(rtp_payload(&z), None);
        // Non-RTP-v2 datagram (e.g. a stray non-RTP packet) → rejected.
        let mut v = rtp(true, 1, 1, &[0xAA]);
        v[0] &= 0x3F;
        assert_eq!(rtp_payload(&v), None);
    }

    #[test]
    fn stale_retransmit_dropped_but_restart_accepted() {
        let mut d = Depacketizer::new(Codec::H264);
        let nal = [0x65u8, 0xAA];
        assert!(d.push(&rtp(true, 10_000, 1, &nal)).is_some());
        // A late NACK retransmit from an older frame must be ignored (its frame is
        // long flushed) — and must NOT disturb the next frame's assembly.
        assert!(d.push(&rtp(true, 7_000, 0, &nal)).is_none());
        let au = d.push(&rtp(true, 11_500, 2, &nal)).expect("next frame unaffected");
        assert_eq!(au, annexb(&[&nal]));
        // An encoder restart lands on a fresh random ts base (huge backward jump) —
        // accepted, stream resyncs.
        assert!(d.push(&rtp(true, 11_500u32.wrapping_sub(5_000_000), 3, &nal)).is_some());
    }

    #[test]
    fn h264_single_nal() {
        let mut d = Depacketizer::new(Codec::H264);
        let nal = [0x65u8, 0xAA, 0xBB]; // type 5 (IDR)
        let au = d.push(&rtp(true, 100, 1, &nal)).expect("au");
        assert_eq!(au, annexb(&[&nal]));
    }

    #[test]
    fn h264_stap_a() {
        let mut d = Depacketizer::new(Codec::H264);
        let sps = [0x67u8, 0x42, 0x00];
        let pps = [0x68u8, 0xCE, 0x3C];
        let mut payload = vec![0x18u8]; // STAP-A (24)
        payload.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        payload.extend_from_slice(&sps);
        payload.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        payload.extend_from_slice(&pps);
        let au = d.push(&rtp(true, 1, 1, &payload)).expect("au");
        assert_eq!(au, annexb(&[&sps, &pps]));
    }

    #[test]
    fn h264_fu_a_reassembly() {
        let mut d = Depacketizer::new(Codec::H264);
        // Fragment a type-5 NAL "HELLO" across two packets, NRI=3.
        let p1 = [0x7Cu8, 0x85, b'H', b'E']; // ind=F0|NRI3|28=0x7C, hdr S=1 type=5
        let p2 = [0x7Cu8, 0x45, b'L', b'L', b'O']; // hdr E=1 type=5
        assert!(d.push(&rtp(false, 7, 1, &p1)).is_none());
        let au = d.push(&rtp(true, 7, 2, &p2)).expect("au");
        assert_eq!(au, annexb(&[&[0x65, b'H', b'E', b'L', b'L', b'O']]));
    }

    #[test]
    fn h265_single_nal() {
        let mut d = Depacketizer::new(Codec::H265);
        let nal = [0x26u8, 0x01, 0xAA]; // type (0x26>>1)&0x3f = 19 (IDR_W_RADL)
        let au = d.push(&rtp(true, 5, 1, &nal)).expect("au");
        assert_eq!(au, annexb(&[&nal]));
    }

    #[test]
    fn h265_fu_reassembly() {
        let mut d = Depacketizer::new(Codec::H265);
        // FU type 49 (0x62,0x01), fragmented NAL of type 19, payload "AB"/"CD".
        let p1 = [0x62u8, 0x01, 0x93, b'A', b'B']; // hdr S=1 (0x80) | type19 (0x13) = 0x93
        let p2 = [0x62u8, 0x01, 0x53, b'C', b'D']; // hdr E=1 (0x40) | type19 = 0x53
        assert!(d.push(&rtp(false, 9, 1, &p1)).is_none());
        let au = d.push(&rtp(true, 9, 2, &p2)).expect("au");
        // reconstructed header byte0 = (0x62 & 0x81) | (19<<1) = 0x00|0x26 = 0x26
        assert_eq!(au, annexb(&[&[0x26, 0x01, b'A', b'B', b'C', b'D']]));
    }

    #[test]
    fn marker_groups_multiple_nals_into_one_au() {
        let mut d = Depacketizer::new(Codec::H264);
        let a = [0x41u8, 1, 2]; // non-IDR slice
        let b = [0x41u8, 3, 4];
        assert!(d.push(&rtp(false, 50, 1, &a)).is_none());
        let au = d.push(&rtp(true, 50, 2, &b)).expect("au");
        assert_eq!(au, annexb(&[&a, &b]));
    }

    #[test]
    fn timestamp_change_flushes_unmarked_frame() {
        let mut d = Depacketizer::new(Codec::H264);
        let a = [0x41u8, 1]; // frame 1, marker lost
        let b = [0x41u8, 2]; // frame 2
        assert!(d.push(&rtp(false, 1, 1, &a)).is_none());
        // new ts → previous AU flushed
        let au = d.push(&rtp(false, 2, 2, &b)).expect("flushed prev au");
        assert_eq!(au, annexb(&[&a]));
    }

    // -------------------------------------------------------------------------
    // LEB128 tests
    // -------------------------------------------------------------------------

    #[test]
    fn leb128_single_byte() {
        assert_eq!(read_leb128(&[0x00]), Some((0, 1)));
        assert_eq!(read_leb128(&[0x7F]), Some((127, 1)));
    }

    #[test]
    fn leb128_multibyte() {
        // 300 = 0x12C → LEB128: 0xAC 0x02
        assert_eq!(read_leb128(&[0xAC, 0x02]), Some((300, 2)));
        // 16384 = 0x4000 → LEB128: 0x80 0x80 0x01
        assert_eq!(read_leb128(&[0x80, 0x80, 0x01]), Some((16384, 3)));
    }

    #[test]
    fn leb128_empty_is_none() {
        assert_eq!(read_leb128(&[]), None);
    }

    #[test]
    fn leb128_truncated_is_none() {
        // High bit set on last byte means more bytes expected.
        assert_eq!(read_leb128(&[0x80]), None);
    }

    // -------------------------------------------------------------------------
    // AV1 RFC 9043 tests
    // -------------------------------------------------------------------------

    /// Build a minimal AV1 OBU aggregation header byte.
    /// z=continuation, y=fragmented-last, w=element-count (0..=7), n=new-sequence.
    fn av1_agg_hdr(z: bool, y: bool, w: u8, n: bool) -> u8 {
        let mut b = 0u8;
        if z { b |= 0x80; }
        if y { b |= 0x40; }
        b |= (w & 0x07) << 3;
        if n { b |= 0x04; }
        b
    }

    /// Build an AV1 RTP payload: [agg_hdr][elem...]
    /// Each elem is `(obu_bytes, include_length_prefix)`.
    fn av1_payload(hdr: u8, elems: &[(&[u8], bool)]) -> Vec<u8> {
        let mut v = vec![hdr];
        for (obu, with_len) in elems {
            if *with_len {
                // LEB128-encode the length.
                let mut n = obu.len();
                loop {
                    let mut byte = (n & 0x7F) as u8;
                    n >>= 7;
                    if n != 0 { byte |= 0x80; }
                    v.push(byte);
                    if n == 0 { break; }
                }
            }
            v.extend_from_slice(obu);
        }
        v
    }

    #[test]
    fn av1_single_obu_whole_packet() {
        // W=1: one element, last element → no length prefix.
        // Aggregation header: Z=0, Y=0, W=1, N=1 (new sequence).
        let obu = [0x0Au8, 0x00, 0xAA, 0xBB]; // sequence header OBU (type 1, hdr=0x0A)
        let hdr = av1_agg_hdr(false, false, 1, true);
        let payload = av1_payload(hdr, &[(&obu, false)]); // W=1 last elem: no len prefix
        let mut d = Depacketizer::new(Codec::Av1);
        let au = d.push(&rtp(true, 1, 1, &payload)).expect("av1 au");
        assert_eq!(au, obu);
    }

    #[test]
    fn av1_two_obus_in_one_packet() {
        // W=2: two OBU elements; first has LEB128 length, second (last) does not.
        let obu_a = [0x0Au8, 0x01]; // sequence header OBU body (2 bytes)
        let obu_b = [0x32u8, 0xAA, 0xBB, 0xCC]; // frame OBU (type 6)
        let hdr = av1_agg_hdr(false, false, 2, false);
        // First elem with length prefix (2 bytes), second without.
        let payload = av1_payload(hdr, &[(&obu_a, true), (&obu_b, false)]);
        let mut d = Depacketizer::new(Codec::Av1);
        let au = d.push(&rtp(true, 2, 1, &payload)).expect("av1 two-obu au");
        let mut expected = obu_a.to_vec();
        expected.extend_from_slice(&obu_b);
        assert_eq!(au, expected);
    }

    #[test]
    fn av1_fragmented_obu_across_two_packets() {
        // Packet 1: Z=0, Y=1, W=1 (last elem has no length prefix).
        //   Contains the first half of a frame OBU (header byte + data).
        let obu_hdr_byte = 0x32u8; // frame OBU header (type 6, no extension)
        let part1 = [obu_hdr_byte, b'H', b'E', b'L'];
        let hdr1 = av1_agg_hdr(false, true, 1, false);
        let payload1 = av1_payload(hdr1, &[(&part1, false)]);

        // Packet 2: Z=1, Y=0, W=1 (continuation + last, no length prefix).
        //   Contains the second half (tail of the OBU, WITHOUT re-emitting the header).
        let part2 = [b'L', b'O'];
        let hdr2 = av1_agg_hdr(true, false, 1, false);
        let payload2 = av1_payload(hdr2, &[(&part2, false)]);

        let mut d = Depacketizer::new(Codec::Av1);
        // First packet: Y=1 so last element is fragmented → starts accumulation.
        assert!(d.push(&rtp(false, 10, 1, &payload1)).is_none());
        // Second packet: Z=1, marker=true → continuation fragment + emit TU.
        let au = d.push(&rtp(true, 10, 2, &payload2)).expect("av1 frag au");

        // The reassembled OBU: header byte + full body (part1[1..] + part2).
        let mut expected = vec![obu_hdr_byte];
        expected.extend_from_slice(&part1[1..]);
        expected.extend_from_slice(&part2);
        assert_eq!(au, expected);
    }

    #[test]
    fn av1_w0_multiple_obus_leb128_all() {
        // W=0: element count not signalled; all elements are length-prefixed.
        // Two OBUs: [len_a][obu_a][len_b][obu_b]  (last must still have length when W=0)
        let obu_a = [0x0Cu8, 0x00]; // metadata OBU (type 2, hdr byte)
        let obu_b = [0x20u8, 0xAA, 0xBB]; // frame header OBU (type 4)
        let hdr = av1_agg_hdr(false, false, 0, false);
        let payload = av1_payload(hdr, &[(&obu_a, true), (&obu_b, true)]);
        let mut d = Depacketizer::new(Codec::Av1);
        let au = d.push(&rtp(true, 5, 1, &payload)).expect("av1 w0 au");
        let mut expected = obu_a.to_vec();
        expected.extend_from_slice(&obu_b);
        assert_eq!(au, expected);
    }

    #[test]
    fn av1_timestamp_change_flushes_av1_frame() {
        // Send first frame without marker (lost), then a second frame.
        // The timestamp change should flush the first frame's OBU.
        let obu1 = [0x32u8, 0xAA];
        let obu2 = [0x32u8, 0xBB];
        let hdr = av1_agg_hdr(false, false, 1, false);
        let p1 = av1_payload(hdr, &[(&obu1, false)]);
        let p2 = av1_payload(hdr, &[(&obu2, false)]);
        let mut d = Depacketizer::new(Codec::Av1);
        assert!(d.push(&rtp(false, 1, 1, &p1)).is_none());
        let au = d.push(&rtp(false, 2, 2, &p2)).expect("flushed av1 au");
        assert_eq!(au, obu1); // first frame was flushed
    }

    // -------------------------------------------------------------------------
    // A7 ReorderBuffer + keyframe-detection tests
    // -------------------------------------------------------------------------

    #[test]
    fn reorder_in_order_passes_through() {
        let mut rb = ReorderBuffer::new();
        let now = Instant::now();
        rb.push(10, &[0xAA]);
        rb.push(11, &[0xBB]);
        rb.push(12, &[0xCC]);
        assert_eq!(rb.pop_ready(now), Some(vec![0xAA]));
        assert_eq!(rb.pop_ready(now), Some(vec![0xBB]));
        assert_eq!(rb.pop_ready(now), Some(vec![0xCC]));
        assert_eq!(rb.pop_ready(now), None);
        assert!(!rb.take_gap_declared());
    }

    #[test]
    fn reorder_holds_out_of_order_then_releases() {
        let mut rb = ReorderBuffer::new();
        let now = Instant::now();
        rb.push(1, &[1]);
        rb.push(3, &[3]); // gap at 2
        assert_eq!(rb.pop_ready(now), Some(vec![1]));
        assert_eq!(rb.pop_ready(now), None, "2 still missing, within window");
        rb.push(2, &[2]); // the missing packet arrives (reordered)
        assert_eq!(rb.pop_ready(now), Some(vec![2]));
        assert_eq!(rb.pop_ready(now), Some(vec![3]));
        assert!(!rb.take_gap_declared(), "filled before timeout → no gap");
        assert_eq!(rb.take_loss_window(), (3, 0), "reorder is not loss");
    }

    #[test]
    fn reorder_timeout_declares_gap() {
        let mut rb = ReorderBuffer::new();
        let t0 = Instant::now();
        rb.push(1, &[1]);
        rb.push(3, &[3]);
        assert_eq!(rb.pop_ready(t0), Some(vec![1]));
        assert_eq!(rb.pop_ready(t0), None); // arms the wait timer
        // Not before the wait (the 250 ms bound until an RTT sized it)...
        assert_eq!(rb.pop_ready(t0 + Duration::from_millis(200)), None);
        let later = t0 + Duration::from_millis(260);
        assert_eq!(rb.pop_ready(later), Some(vec![3]), "flush past the hole");
        assert!(rb.take_gap_declared(), "skipping seq 2 declared a gap");
        assert_eq!(rb.take_loss_window().1, 1, "one truly-lost packet");
    }

    #[test]
    fn reorder_wait_follows_the_repair_loop_bounded() {
        let mut rb = ReorderBuffer::new();
        assert_eq!(rb.max_delay(), Duration::from_millis(250));
        rb.set_max_delay_ms(80.0);
        assert!((rb.max_delay().as_secs_f32() * 1000.0 - 80.0).abs() < 0.01);
        rb.set_max_delay_ms(900.0);
        assert_eq!(rb.max_delay(), Duration::from_millis(250), "never the old 500 ms");
        rb.set_max_delay_ms(f32::NAN);
        assert_eq!(rb.max_delay(), Duration::from_millis(250));
        // A short wait flushes a hole sooner.
        rb.set_max_delay_ms(60.0);
        let t0 = Instant::now();
        rb.push(1, &[1]);
        rb.push(3, &[3]);
        assert_eq!(rb.pop_ready(t0), Some(vec![1]));
        assert_eq!(rb.pop_ready(t0), None);
        assert_eq!(rb.pop_ready(t0 + Duration::from_millis(61)), Some(vec![3]));
        assert!(rb.take_gap_declared());
    }

    /// At 5k packets/s (~50 Mbit) a hole with a 120 ms wait holds ~600 packets: it is
    /// released by its timeout (the repair window), not by the overflow guard, and the
    /// packets queued far ahead of the cursor are not mistaken for an encoder restart.
    #[test]
    fn reorder_high_rate_hole_waits_its_window() {
        let mut rb = ReorderBuffer::new();
        rb.set_max_delay_ms(120.0);
        let t0 = Instant::now();
        rb.push(1000, &[0]);
        assert_eq!(rb.pop_ready(t0), Some(vec![0]));
        // seq 1001 is lost; 600 packets (120 ms at 5k/s) arrive behind it.
        for i in 0..600u16 {
            rb.push(1002 + i, &[1]);
            if i % 50 == 0 {
                let t = t0 + Duration::from_micros(200 * i as u64);
                assert_eq!(rb.pop_ready(t), None, "held at packet {i}");
            }
        }
        assert!(!rb.take_gap_declared(), "no overflow flush, no false resync");
        // The retransmit lands inside the window: no gap at all.
        rb.push(1001, &[2]);
        assert_eq!(rb.pop_ready(t0 + Duration::from_millis(110)), Some(vec![2]));
        let mut n = 0;
        while rb.pop_ready(t0 + Duration::from_millis(110)).is_some() {
            n += 1;
        }
        assert_eq!(n, 600);
        assert!(!rb.take_gap_declared());
        assert_eq!(rb.take_loss_window().1, 0);
        // An unrepaired hole at that rate is released by the timeout.
        rb.push(1603, &[3]);
        for i in 0..600u16 {
            rb.push(1604 + i, &[4]);
        }
        let t1 = t0 + Duration::from_millis(200);
        assert_eq!(rb.pop_ready(t1), None, "arms the wait");
        assert_eq!(rb.pop_ready(t1 + Duration::from_millis(100)), None);
        assert!(rb.pop_ready(t1 + Duration::from_millis(121)).is_some(), "timeout flush");
        assert!(rb.take_gap_declared());
        assert_eq!(rb.take_loss_window().1, 1);
    }

    /// A fresh RTP base (encoder restart) still resyncs, ahead or behind.
    #[test]
    fn reorder_resyncs_on_a_restart_but_not_on_a_retransmit() {
        let mut rb = ReorderBuffer::new();
        let now = Instant::now();
        rb.push(100, &[1]);
        assert_eq!(rb.pop_ready(now), Some(vec![1]));
        rb.push(40_000, &[2]); // far ahead of the newest: restart
        assert!(rb.take_gap_declared());
        assert_eq!(rb.pop_ready(now), Some(vec![2]));
        rb.push(39_000, &[3]); // far behind the cursor: restart
        assert!(rb.take_gap_declared());
        assert_eq!(rb.pop_ready(now), Some(vec![3]));
        // A hole with 400 packets behind it; its retransmit is 400 behind the newest.
        for i in 2..402u16 {
            rb.push(39_000 + i, &[4]);
        }
        rb.push(39_001, &[5]);
        assert!(!rb.take_gap_declared(), "a late retransmit is this stream");
        assert_eq!(rb.pop_ready(now), Some(vec![5]));
    }

    #[test]
    fn reorder_dedup_drops_duplicates() {
        let mut rb = ReorderBuffer::new();
        let now = Instant::now();
        rb.push(5, &[5]);
        rb.push(5, &[0xFF]); // duplicate seq → dropped
        assert_eq!(rb.pop_ready(now), Some(vec![5]));
        assert_eq!(rb.pop_ready(now), None);
    }

    #[test]
    fn reorder_reset_clears_state() {
        let mut rb = ReorderBuffer::new();
        let now = Instant::now();
        rb.push(100, &[1]);
        rb.reset();
        // After reset the next packet re-primes the cursor at its own seq.
        rb.push(7, &[7]);
        assert_eq!(rb.pop_ready(now), Some(vec![7]));
    }

    #[test]
    fn au_is_keyframe_h264() {
        let idr = annexb(&[&[0x65u8, 0x11]]); // NAL type 5 = IDR
        assert!(au_is_keyframe(Codec::H264, &idr));
        let p = annexb(&[&[0x41u8, 0x22]]); // NAL type 1 = non-IDR slice
        assert!(!au_is_keyframe(Codec::H264, &p));
    }

    #[test]
    fn au_is_keyframe_h265() {
        let idr = annexb(&[&[0x26u8, 0x01]]); // (0x26>>1)&0x3f = 19 = IDR_W_RADL
        assert!(au_is_keyframe(Codec::H265, &idr));
        let trail = annexb(&[&[0x02u8, 0x01]]); // type 1 = TRAIL_R (non-IRAP)
        assert!(!au_is_keyframe(Codec::H265, &trail));
    }

    /// H.264 recovery-tag SEI NAL (user data unregistered, the host's UUID).
    fn tag_sei_h264() -> Vec<u8> {
        let mut n = vec![0x06, 5, 16];
        n.extend_from_slice(&RFI_UUID);
        n.push(0x80);
        n
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn recovery_tag_is_found_in_packets_and_access_units() {
        let sei = tag_sei_h264();
        assert!(is_recovery_tag_packet(&rtp(false, 1, 1, &sei)));
        assert!(!is_recovery_tag_packet(&rtp(false, 1, 1, &[0x41, 0x22])));
        let mut hevc = vec![39 << 1, 0x01, 5, 16];
        hevc.extend_from_slice(&RFI_UUID);
        assert!(is_recovery_tag_packet(&rtp(false, 1, 1, &hevc)));
        assert!(au_has_recovery_tag(Codec::H264, &annexb(&[&sei, &[0x41, 0x22]])));
        assert!(au_has_recovery_tag(Codec::H265, &annexb(&[&hevc, &[0x02, 0x01]])));
        assert!(!au_has_recovery_tag(Codec::H264, &annexb(&[&[0x06, 5, 16, 1, 2], &[0x41, 0x22]])));
        assert!(!au_has_recovery_tag(Codec::Av1, &sei));
    }

    #[test]
    fn the_recovery_wait_grows_with_a_slow_round_trip() {
        assert_eq!(rfi_timeout(None), RFI_TIMEOUT);
        assert_eq!(rfi_timeout(Some(ms(40))), RFI_TIMEOUT);
        assert_eq!(rfi_timeout(Some(ms(300))), ms(500));
        assert_eq!(rfi_timeout(Some(Duration::from_secs(2))), RFI_GIVE_UP);
    }

    /// RFI: a hole is reported after `RFI_REPORT_AFTER` (while repair still runs), and the
    /// host's recovery tag held behind it releases past it at once — no repair-window wait.
    #[test]
    fn rfi_reports_the_hole_and_releases_at_the_recovery_tag() {
        let t0 = Instant::now();
        let mut rb = ReorderBuffer::new();
        rb.set_rfi(true);
        rb.set_max_delay_ms(250.0);
        let slice = [0x41u8, 0x22];
        rb.push_at(10, &rtp(true, 1, 10, &slice), t0);
        assert!(rb.pop_ready(t0).is_some());
        // 11 lost; 12 (frame 2) arrives.
        rb.push_at(12, &rtp(true, 2, 12, &slice), t0);
        rb.poll_rfi(t0 + ms(1));
        assert_eq!(rb.take_rfi_request(), None, "not before RFI_REPORT_AFTER (reordering)");
        assert_eq!(rb.pop_ready(t0 + ms(1)), None, "held for the repair");
        rb.poll_rfi(t0 + ms(4));
        assert_eq!(rb.take_rfi_request(), Some((11, 11)), "reported while repair still runs");
        // The recovery frame: its tag packet, then its slice.
        rb.push_at(13, &rtp(false, 3, 13, &tag_sei_h264()), t0 + ms(20));
        rb.push_at(14, &rtp(true, 3, 14, &slice), t0 + ms(20));
        rb.poll_rfi(t0 + ms(20));
        assert_eq!(rb.take_rfi_request(), None, "a reported hole is not reported twice");
        let got: Vec<u16> = std::iter::from_fn(|| rb.pop_ready(t0 + ms(20)))
            .map(|p| u16::from_be_bytes([p[2], p[3]]))
            .collect();
        assert_eq!(got, vec![12, 13, 14], "released at once, long before the 250 ms window");
        assert!(rb.take_gap_declared());
        assert!(!rb.take_resynced());
    }

    /// RFI: a fresh (unreported) hole in front of a tag is not skipped — it is most likely a
    /// packet still on its way.
    #[test]
    fn rfi_tag_does_not_skip_an_unreported_hole() {
        let t0 = Instant::now();
        let mut rb = ReorderBuffer::new();
        rb.set_rfi(true);
        rb.set_max_delay_ms(250.0);
        rb.push_at(1, &rtp(true, 1, 1, &[0x41, 0x22]), t0);
        assert!(rb.pop_ready(t0).is_some());
        rb.push_at(3, &rtp(false, 3, 3, &tag_sei_h264()), t0);
        assert_eq!(rb.pop_ready(t0 + ms(1)), None);
        assert!(!rb.take_gap_declared());
    }

    /// RFI on a reordering path: a hole is reported only once it waited out the lateness the
    /// path showed (else every late packet was "lost" and answered with a recovery frame).
    #[test]
    fn rfi_report_waits_out_the_paths_reordering() {
        let t0 = Instant::now();
        let mut rb = ReorderBuffer::new();
        rb.set_rfi(true);
        let p = |seq: u16| rtp(true, seq as u32, seq, &[0x41, 0x22]);
        rb.push_at(1, &p(1), t0);
        rb.push_at(3, &p(3), t0);
        // 2 comes 40 ms late: the path reorders.
        rb.push_at(2, &p(2), t0 + ms(40));
        while rb.pop_ready(t0 + ms(40)).is_some() {}
        rb.poll_rfi(t0 + ms(40));
        assert_eq!(rb.take_rfi_request(), None, "filled, never reported");
        // 4 lost, 5 arrives: a 30 ms old hole is reordering on this path…
        rb.push_at(5, &p(5), t0 + ms(40));
        rb.poll_rfi(t0 + ms(70));
        assert_eq!(rb.take_rfi_request(), None);
        // …reported after 1.25 × the lateness (50 ms).
        rb.poll_rfi(t0 + ms(91));
        assert_eq!(rb.take_rfi_request(), Some((4, 4)));
    }

    /// RFI off: nothing is reported, the tag releases nothing early.
    #[test]
    fn rfi_off_reports_nothing() {
        let t0 = Instant::now();
        let mut rb = ReorderBuffer::new();
        rb.set_max_delay_ms(250.0);
        rb.push_at(1, &rtp(true, 1, 1, &[0x41, 0x22]), t0);
        assert!(rb.pop_ready(t0).is_some());
        rb.push_at(3, &rtp(false, 3, 3, &tag_sei_h264()), t0);
        rb.poll_rfi(t0 + ms(50));
        assert_eq!(rb.take_rfi_request(), None);
        assert_eq!(rb.pop_ready(t0 + ms(50)), None);
    }

    /// The gate: after a hole only the whole recovery frame (or a keyframe) resumes decoding.
    #[test]
    fn rfi_gate_resumes_at_a_whole_recovery_frame() {
        let t0 = Instant::now();
        let mut g = RfiGate::new();
        let p_frame = annexb(&[&[0x41, 0x22]]);
        let recovery = annexb(&[&tag_sei_h264(), &[0x41, 0x22]]);
        assert!(g.admit(Codec::H264, &p_frame, 1, t0, None), "no hole: decodes");
        // A hole ran into frame 2.
        g.on_hole(&rtp(true, 2, 5, &[0x41]), t0);
        assert!(!g.admit(Codec::H264, &p_frame, 2, t0, None), "the torn AU never decodes");
        assert!(!g.admit(Codec::H264, &p_frame, 3, t0, None), "predicted from the lost frame");
        assert!(!g.wants_keyframe(), "no keyframe while the recovery frame is due");
        assert!(g.admit(Codec::H264, &recovery, 4, t0 + ms(30), None), "the recovery frame decodes");
        assert!(g.admit(Codec::H264, &p_frame, 5, t0 + ms(40), None));
    }

    /// The gate: a recovery frame the hole ran through is not accepted.
    #[test]
    fn rfi_gate_rejects_a_damaged_recovery_frame() {
        let t0 = Instant::now();
        let mut g = RfiGate::new();
        let recovery = annexb(&[&tag_sei_h264(), &[0x41, 0x22]]);
        g.on_hole(&rtp(true, 4, 9, &[0x41]), t0);
        assert!(!g.admit(Codec::H264, &recovery, 4, t0, None));
        // A hole that ends right at the tag packet did not run through the frame.
        g.on_hole(&rtp(false, 6, 12, &tag_sei_h264()), t0);
        assert!(g.admit(Codec::H264, &recovery, 6, t0, None));
    }

    /// The gate: no recovery frame in time → the keyframe; a whole recovery frame still ends
    /// that keyframe wait (the fallback), a stream restart's wait does not.
    #[test]
    fn rfi_gate_falls_back_to_a_keyframe() {
        let t0 = Instant::now();
        let mut g = RfiGate::new();
        let p_frame = annexb(&[&[0x41, 0x22]]);
        let recovery = annexb(&[&tag_sei_h264(), &[0x41, 0x22]]);
        let idr = annexb(&[&[0x67, 1], &[0x65, 0x11]]);
        g.on_hole(&rtp(true, 2, 5, &[0x41]), t0);
        assert!(!g.poll(t0 + ms(100), None));
        assert!(g.poll(t0 + RFI_TIMEOUT, None), "timed out");
        assert!(g.wants_keyframe());
        assert!(!g.admit(Codec::H264, &p_frame, 3, t0 + ms(200), None));
        assert!(g.admit(Codec::H264, &recovery, 4, t0 + ms(210), None), "fallback: recovery frame");
        assert!(!g.wants_keyframe());
        // Encoder restart: only the keyframe.
        g.on_resync(&rtp(true, 100, 1, &[0x41]));
        assert!(!g.admit(Codec::H264, &recovery, 101, t0, None));
        assert!(!g.admit(Codec::H264, &idr, 100, t0, None), "the restart's first AU may be torn");
        assert!(g.admit(Codec::H264, &idr, 102, t0, None));
    }

    /// The gate: every further hole restarts the timeout, up to `RFI_GIVE_UP` in all.
    #[test]
    fn rfi_gate_gives_up_after_a_second() {
        let t0 = Instant::now();
        let mut g = RfiGate::new();
        let mut t = t0;
        while t < t0 + RFI_GIVE_UP {
            g.on_hole(&rtp(true, 2, 5, &[0x41]), t);
            assert!(!g.wants_keyframe());
            t += ms(100);
        }
        assert!(g.poll(t0 + RFI_GIVE_UP, None));
    }

    #[test]
    fn depacketizer_reports_the_au_timestamp_and_drops_partials() {
        let mut d = Depacketizer::new(Codec::H264);
        assert!(d.push(&rtp(false, 7, 1, &[0x41, 0x01])).is_none());
        d.drop_partial();
        assert!(d.push(&rtp(true, 7, 3, &[0x41, 0x02])).is_some());
        assert_eq!(d.au_ts(), 7);
        // A ts change flushes the unmarked previous AU: its own timestamp is reported.
        assert!(d.push(&rtp(false, 8, 4, &[0x41, 0x03])).is_none());
        assert!(d.push(&rtp(false, 9, 5, &[0x41, 0x04])).is_some());
        assert_eq!(d.au_ts(), 8);
    }
}
