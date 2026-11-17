package dev.pulsar.video

import android.content.Context

/**
 * Notification strings in the APP's language (tr/en/ru/kk — the JS i18n catalogs),
 * not the device locale: the user can pick a language in Settings that differs from
 * the system one, and notifications must match the rest of the UI. The JS layer
 * pushes its active language via the `set_notif_lang` command whenever it changes;
 * it's persisted in SharedPreferences so services (HostService) read it too.
 * Before the JS layer has pushed a language, the device locale is used when a
 * translation exists for it; English is the default and the fallback.
 * Strings sent to a peer never come from here — they are always English.
 */
object NotifI18n {
    private const val PREFS = "pulsar-notif"
    private const val KEY = "lang"

    fun setLang(ctx: Context, lang: String) {
        ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit().putString(KEY, lang).apply()
    }

    fun lang(ctx: Context): String =
        ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getString(KEY, null)
            ?: java.util.Locale.getDefault().language

    fun t(ctx: Context, key: String): String {
        val l = lang(ctx)
        return when (key) {
            "chanRequests" -> when (l) {
                "tr" -> "Bağlantı istekleri"; "ru" -> "Запросы на подключение"
                "kk" -> "Қосылу сұраулары"; else -> "Connection requests"
            }
            "reqTitle" -> when (l) {
                "tr" -> "Bağlantı isteği"; "ru" -> "Запрос на подключение"
                "kk" -> "Қосылу сұрауы"; else -> "Connection request"
            }
            "reqText" -> when (l) {
                "tr" -> "Onaylamak için dokun"; "ru" -> "Нажмите, чтобы одобрить"
                "kk" -> "Мақұлдау үшін түртіңіз"; else -> "Tap to approve"
            }
            "chanHost" -> when (l) {
                "tr" -> "Ekran paylaşımı"; "ru" -> "Трансляция экрана"
                "kk" -> "Экран бөлісу"; else -> "Screen sharing"
            }
            "chanHostAlert" -> when (l) {
                "tr" -> "Ekran paylaşımı uyarıları"; "ru" -> "Оповещения трансляции экрана"
                "kk" -> "Экран бөлісу ескертулері"; else -> "Screen sharing alerts"
            }
            "sharing" -> when (l) {
                "tr" -> "Ekran paylaşılıyor"; "ru" -> "Экран транслируется"
                "kk" -> "Экран бөлісілуде"; else -> "Screen is being shared"
            }
            "stopSharing" -> when (l) {
                "tr" -> "Paylaşımı durdur"; "ru" -> "Остановить"
                "kk" -> "Тоқтату"; else -> "Stop sharing"
            }
            "projStoppedTitle" -> when (l) {
                "tr" -> "Ekran paylaşımı durdu"; "ru" -> "Трансляция экрана остановлена"
                "kk" -> "Экран бөлісу тоқтады"; else -> "Screen sharing stopped"
            }
            "projStoppedText" -> when (l) {
                "tr" -> "Paylaşım telefonda durduruldu ve bağlantı kapandı. Biri yeniden bağlandığında izin tekrar istenecek."
                "ru" -> "Трансляция остановлена на телефоне, подключение закрыто. При новом подключении разрешение будет запрошено снова."
                "kk" -> "Бөлісу телефонда тоқтатылды, қосылым жабылды. Қайта қосылғанда рұқсат қайтадан сұралады."
                else -> "Sharing was stopped on the phone and the connection closed. Permission is asked again on the next connection."
            }
            "streamActive" -> when (l) {
                "tr" -> "Uzak oturum aktif"; "ru" -> "Удалённый сеанс активен"
                "kk" -> "Қашықтағы сеанс белсенді"; else -> "Remote session active"
            }
            "chanFiles" -> when (l) {
                "tr" -> "Alınan dosyalar"; "ru" -> "Полученные файлы"
                "kk" -> "Алынған файлдар"; else -> "Received files"
            }
            "fileRecvTitle" -> when (l) {
                "tr" -> "Dosya alındı"; "ru" -> "Файл получен"
                "kk" -> "Файл алынды"; else -> "File received"
            }
            "fileOpen" -> when (l) {
                "tr" -> "Aç"; "ru" -> "Открыть"
                "kk" -> "Ашу"; else -> "Open"
            }
            "fileShare" -> when (l) {
                "tr" -> "Paylaş"; "ru" -> "Поделиться"
                "kk" -> "Бөлісу"; else -> "Share"
            }
            else -> key
        }
    }
}
