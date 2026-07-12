package dev.pulsar.video

import android.content.Context

/**
 * Notification strings in the APP's language (tr/en/ru/kk — the JS i18n catalogs),
 * not the device locale: the user can pick a language in Settings that differs from
 * the system one, and notifications must match the rest of the UI. The JS layer
 * pushes its active language via the `set_notif_lang` command whenever it changes;
 * it's persisted in SharedPreferences so services (HostService) read it too.
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
            "sharing" -> when (l) {
                "tr" -> "Ekran paylaşılıyor"; "ru" -> "Экран транслируется"
                "kk" -> "Экран бөлісілуде"; else -> "Screen is being shared"
            }
            "stopSharing" -> when (l) {
                "tr" -> "Paylaşımı durdur"; "ru" -> "Остановить"
                "kk" -> "Тоқтату"; else -> "Stop sharing"
            }
            else -> key
        }
    }
}
