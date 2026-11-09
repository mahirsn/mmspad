package net.mahirsn.mmspad

import android.content.Context
import android.content.SharedPreferences

class PreferencesManager(context: Context) {
    private val prefs: SharedPreferences = context.getSharedPreferences("prefs", Context.MODE_PRIVATE)

    /** Fingers work as the PC's touchpad; off by default, so a resting hand does not move the cursor. */
    var touchEnabled: Boolean
        get() = prefs.getBoolean("touch_enabled", false)
        set(value) = prefs.edit().putBoolean("touch_enabled", value).apply()

    /** Show the PC screen on the tablet; off by default (the PC then encodes nothing). */
    var showScreen: Boolean
        get() = prefs.getBoolean("show_screen", false)
        set(value) = prefs.edit().putBoolean("show_screen", value).apply()
}
