package com.horizon.castsender.pages

import android.content.Intent
import android.view.View
import com.horizon.castsender.MainActivity
import com.horizon.castsender.ui.dp

/** One tab. The activity owns the instances so state survives tab switches. */
abstract class Page(val activity: MainActivity) {
    abstract val root: View

    /** Keeps `dp(...)` working inside nested view `apply {}` blocks. */
    fun dp(value: Int): Int = activity.dp(value)

    open fun onShow() {}

    open fun onHide() {}

    /** Called twice a second while this page is visible. */
    open fun onTick() {}

    /** The activity is visible again after a dialog or another app paused it. */
    open fun onActivityResume() {}

    open fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {}

    open fun onDestroy() {}
}
