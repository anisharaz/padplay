// SPDX-License-Identifier: Apache-2.0

package com.moreland.display

import android.animation.Animator
import android.animation.AnimatorListenerAdapter
import android.animation.ValueAnimator
import android.content.Context
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.view.Gravity
import android.view.View
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.TextView

/**
 * A collapsible diagnostics panel tucked against the left screen edge.
 *
 * Collapsed, it's a narrow rounded tab with a colored dot for at-a-glance
 * connection state — visible without covering the picture. Tapping it slides
 * out a panel with the full readout (state, resolution/fps, frame count,
 * last error), so there's always a way to check "is this actually working"
 * without `adb logcat` on the host. Auto-expanded on launch (nothing has
 * streamed yet, so the guidance matters), then auto-collapses once on the
 * first decoded frame — after that it only moves when tapped.
 */
class StatsWidget(context: Context, root: FrameLayout) {

    private val density = context.resources.displayMetrics.density
    private fun dp(v: Int) = (v * density).toInt()

    private val dot = TextView(context).apply {
        text = "●"
        textSize = 18f
        gravity = Gravity.CENTER
    }

    private val tab = FrameLayout(context).apply {
        background = roundedDrawable(BG, rightOnly = true, radius = dp(14).toFloat())
        addView(
            dot,
            FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.WRAP_CONTENT,
                FrameLayout.LayoutParams.WRAP_CONTENT,
                Gravity.CENTER,
            ),
        )
    }

    private val header = TextView(context).apply {
        setTextColor(Color.parseColor("#00E5A0"))
        textSize = 12f
        typeface = Typeface.create(Typeface.MONOSPACE, Typeface.BOLD)
        text = "MORELAND ${versionName(context)}"
    }

    private val body = TextView(context).apply {
        setTextColor(Color.WHITE)
        textSize = 12f
        typeface = Typeface.MONOSPACE
    }

    private val panel = LinearLayout(context).apply {
        orientation = LinearLayout.VERTICAL
        background = roundedDrawable(BG, rightOnly = false, radius = dp(14).toFloat())
        setPadding(dp(14), dp(12), dp(14), dp(12))
        addView(header)
        addView(
            View(context),
            LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, dp(6)),
        )
        addView(body)
        visibility = View.GONE
    }

    private val container = LinearLayout(context).apply {
        orientation = LinearLayout.HORIZONTAL
        addView(tab, LinearLayout.LayoutParams(dp(28), dp(72)))
        addView(panel, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT))
    }

    private var expanded = false
    private var hasAutoCollapsed = false

    init {
        root.addView(
            container,
            FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.WRAP_CONTENT,
                FrameLayout.LayoutParams.WRAP_CONTENT,
            ).apply { gravity = Gravity.START or Gravity.CENTER_VERTICAL },
        )
        tab.setOnClickListener { setExpanded(!expanded) }
        setExpanded(true, animate = false)
    }

    fun setExpanded(newExpanded: Boolean, animate: Boolean = true) {
        if (expanded == newExpanded) return
        expanded = newExpanded
        val params = panel.layoutParams as LinearLayout.LayoutParams

        if (!animate) {
            panel.visibility = if (newExpanded) View.VISIBLE else View.GONE
            params.width =
                if (newExpanded) LinearLayout.LayoutParams.WRAP_CONTENT else 0
            panel.layoutParams = params
            return
        }

        panel.visibility = View.VISIBLE
        panel.measure(View.MeasureSpec.UNSPECIFIED, View.MeasureSpec.UNSPECIFIED)
        val targetWidth = panel.measuredWidth
        val startWidth = if (newExpanded) 0 else panel.width
        val endWidth = if (newExpanded) targetWidth else 0

        ValueAnimator.ofInt(startWidth, endWidth).apply {
            duration = 180
            addUpdateListener {
                params.width = it.animatedValue as Int
                panel.layoutParams = params
            }
            addListener(
                object : AnimatorListenerAdapter() {
                    override fun onAnimationEnd(animation: Animator) {
                        if (!newExpanded) {
                            panel.visibility = View.GONE
                        } else {
                            params.width = LinearLayout.LayoutParams.WRAP_CONTENT
                            panel.layoutParams = params
                        }
                    }
                },
            )
            start()
        }
    }

    /** Called on every refresh tick from [DisplayActivity]. */
    fun update(
        state: String,
        streamInfo: String?,
        fps: Double,
        frames: Long,
        lastError: String?,
        staleMs: Long,
    ) {
        dot.setTextColor(
            when {
                lastError != null && frames == 0L -> COLOR_ERROR
                frames == 0L -> COLOR_WAITING
                staleMs > 3000 -> COLOR_STALLED
                else -> COLOR_LIVE
            },
        )

        body.text = buildString {
            append(state).append('\n')
            streamInfo?.let { append(it).append("  ") }
            append("%.1f fps".format(fps)).append("  ")
            append(frames).append(" frames")
            lastError?.let { append('\n').append(it) }
        }

        if (frames > 0 && !hasAutoCollapsed) {
            hasAutoCollapsed = true
            setExpanded(false)
        }
    }

    /** `versionName@versionCode`, so a rebuilt-and-reinstalled APK is
     * distinguishable on screen from whatever was running before it —
     * useful while iterating without needing adb to check. */
    private fun versionName(context: Context): String = runCatching {
        val info = context.packageManager.getPackageInfo(context.packageName, 0)
        "${info.versionName}@${info.longVersionCode}"
    }.getOrDefault("unknown")

    private fun roundedDrawable(color: Int, rightOnly: Boolean, radius: Float) =
        GradientDrawable().apply {
            shape = GradientDrawable.RECTANGLE
            setColor(color)
            cornerRadii = if (rightOnly) {
                floatArrayOf(0f, 0f, radius, radius, radius, radius, 0f, 0f)
            } else {
                floatArrayOf(radius, radius, radius, radius, radius, radius, radius, radius)
            }
        }

    private companion object {
        val BG = Color.parseColor("#E61A1A1A")
        val COLOR_LIVE = Color.parseColor("#00E5A0")
        val COLOR_STALLED = Color.parseColor("#FFC107")
        val COLOR_WAITING = Color.parseColor("#9E9E9E")
        val COLOR_ERROR = Color.parseColor("#FF5252")
    }
}
