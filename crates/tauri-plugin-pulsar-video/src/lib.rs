use tauri::{
  plugin::{Builder, TauriPlugin},
  Manager, Runtime,
};

pub use models::*;

#[cfg(desktop)]
mod desktop;
#[cfg(mobile)]
mod mobile;

mod commands;
mod error;
mod models;

pub use error::{Error, Result};

/// Mobile host start/stop ordering. Every `stop_host` bumps this; a `start_host` carries the
/// epoch its StreamReq saw, and the plugin drops a start older than the last stop. The start
/// runs on a spawned task (and may wait on the screen-capture permission dialog) while the
/// session teardown stops synchronously, so a late start could otherwise begin capturing —
/// and take the screen wake lock — after its session ended, with nothing left to stop it.
static HOST_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The current host epoch: read it where the stream request arrives, pass it to `start_host`.
pub fn host_epoch() -> u64 {
  HOST_EPOCH.load(std::sync::atomic::Ordering::SeqCst)
}

/// A new epoch for a `stop_host` call (invalidates every earlier start).
#[cfg_attr(desktop, allow(dead_code))]
pub(crate) fn next_host_epoch() -> u64 {
  HOST_EPOCH.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
}

#[cfg(desktop)]
use desktop::PulsarVideo;
#[cfg(mobile)]
use mobile::PulsarVideo;

// W5-native extension trait (HDR / pane layout / decoder enumeration) — mobile-only.
#[cfg(mobile)]
pub use mobile::PulsarVideoW5Ext;

/// Extensions to [`tauri::App`], [`tauri::AppHandle`] and [`tauri::Window`] to access the pulsar-video APIs.
pub trait PulsarVideoExt<R: Runtime> {
  fn pulsar_video(&self) -> &PulsarVideo<R>;
}

impl<R: Runtime, T: Manager<R>> crate::PulsarVideoExt<R> for T {
  fn pulsar_video(&self) -> &PulsarVideo<R> {
    self.state::<PulsarVideo<R>>().inner()
  }
}

/// Initializes the plugin.
pub fn init<R: Runtime>() -> TauriPlugin<R> {
  Builder::new("pulsar-video")
    .invoke_handler(tauri::generate_handler![
      commands::attach,
      commands::detach,
      commands::play_test,
      commands::set_audio_muted,
      commands::set_aspect,
      commands::set_video_transform,
      commands::set_orientation,
      commands::set_status_bar,
      commands::read_clipboard,
      commands::screen_refresh_rate,
      commands::gamepad_battery,
      commands::get_video_size,
      commands::set_pointer_capture,
      commands::get_wallpaper_avatar,
      commands::mic_start,
      commands::mic_stop,
      commands::update_download,
      commands::update_cancel,
      commands::update_status,
      commands::update_install,
    ])
    .setup(|app, api| {
      #[cfg(mobile)]
      let pulsar_video = mobile::init(app, api)?;
      #[cfg(desktop)]
      let pulsar_video = desktop::init(app, api)?;
      app.manage(pulsar_video);
      Ok(())
    })
    .build()
}
