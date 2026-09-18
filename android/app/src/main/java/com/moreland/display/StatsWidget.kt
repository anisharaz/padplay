// SPDX-License-Identifier: Apache-2.0

package com.moreland.display

import android.animation.Animator
import android.animation.AnimatorListenerAdapter
import android.animation.ObjectAnimator
import android.animation.ValueAnimator
import android.content.Context
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.view.Gravity
import android.view.View
import android.view.animation.DecelerateInterpolator
import android.view.animation.OvershootInterpolator
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.TextView

/**
 * A collapsible diagnostics panel tucked against the left screen edge.
 *
 * Collapsed, it's a narrow rounded tab with a pulsing dot for at-a-glance
 * connection state — visible without covering the picture. Tapping it (with
 * a quick press-bounce) slides out a card with the full readout: status, a
 * hero FPS number, resolution, frame count, and how long ago the last frame
 * actually landed — so there's always a way to check "is this working"
 * without `adb logcat` on the host. Auto-expanded on launch (nothing has
 * streamed yet, so the guidance matters), then auto-collapses once on the
 * first decoded frame — after that it only moves when tapped.
 */
// See the same suppression on DisplayActivity: no localization plan for a
// single-purpose diagnostic overlay.
@Suppress("SetTextI18n")
class StatsWidget(private val context: Context, root: FrameLayout) {

    private val density = context.resources.displayMetrics.density
    private fun dp(v: Int) = (v * density).toInt()

    private fun dot(sizeDp: Int) = View(context).apply {
        background = GradientDrawable().apply {
            shape = GradientDrawable.OVAL
            setColor(COLOR_WAITING)
        }
        layoutParams = FrameLayout.LayoutParams(dp(sizeDp), dp(sizeDp))
    }

    private val tabDot = dot(10)
    private val statusDot = dot(9)

    private val tab = FrameLayout(context).apply {
        background = roundedDrawable(BG, rightOnly = true, radius = dp(16).toFloat()).apply {
            alpha = TAB_ALPHA_IDLE
        }
        addView(tabDot, FrameLayout.LayoutParams(WRAP, WRAP, Gravity.CENTER))
    }

    private val versionText = TextView(context).apply {
        setTextColor(Color.parseColor("#5C6672"))
        textSize = 10f
        typeface = Typeface.MONOSPACE
    }

    private val header = LinearLayout(context).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
        addView(
            TextView(context).apply {
                text = "MORELAND"
                setTextColor(COLOR_ACCENT)
                textSize = 12f
                typeface = Typeface.create(Typeface.SANS_SERIF, Typeface.BOLD)
                letterSpacing = 0.12f
                layoutParams = LinearLayout.LayoutParams(0, WRAP, 1f)
            },
        )
        addView(versionText)
    }

    private val divider = View(context).apply {
        setBackgroundColor(Color.parseColor("#332B3540"))
    }

    private val statusLabel = TextView(context).apply {
        setTextColor(Color.WHITE)
        textSize = 14f
        typeface = Typeface.create(Typeface.SANS_SERIF, Typeface.BOLD)
    }

    private val statusRow = LinearLayout(context).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
        addView(statusDot, LinearLayout.LayoutParams(dp(9), dp(9)).apply { rightMargin = dp(8) })
        addView(statusLabel)
    }

    private val streamInfoText = TextView(context).apply {
        setTextColor(Color.parseColor("#8A94A0"))
        textSize = 12f
        typeface = Typeface.MONOSPACE
    }

    private val fpsNumber = TextView(context).apply {
        setTextColor(COLOR_ACCENT)
        textSize = 34f
        typeface = Typeface.create(Typeface.SANS_SERIF, Typeface.BOLD)
    }

    private val fpsCaption = TextView(context).apply {
        text = "fps"
        setTextColor(Color.parseColor("#8A94A0"))
        textSize = 13f
        typeface = Typeface.MONOSPACE
    }

    private val heroRow = LinearLayout(context).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.BOTTOM
        addView(fpsNumber)
        addView(fpsCaption, LinearLayout.LayoutParams(WRAP, WRAP).apply { bottomMargin = dp(6); leftMargin = dp(6) })
    }

    private val framesText = TextView(context).apply {
        setTextColor(Color.parseColor("#8A94A0"))
        textSize = 11f
        typeface = Typeface.MONOSPACE
    }

    private val ageText = TextView(context).apply {
        setTextColor(Color.parseColor("#8A94A0"))
        textSize = 11f
        typeface = Typeface.MONOSPACE
        gravity = Gravity.END
    }

    private val secondaryRow = LinearLayout(context).apply {
        orientation = LinearLayout.HORIZONTAL
        addView(framesText, LinearLayout.LayoutParams(0, WRAP, 1f))
        addView(ageText, LinearLayout.LayoutParams(0, WRAP, 1f))
    }

    private val errorText = TextView(context).apply {
        setTextColor(COLOR_ERROR)
        textSize = 11f
        typeface = Typeface.MONOSPACE
        visibility = View.GONE
    }

    private fun space(heightDp: Int) = View(context).apply {
        layoutParams = LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, dp(heightDp))
    }

    private val panel = LinearLayout(context).apply {
        orientation = LinearLayout.VERTICAL
        background = roundedDrawable(BG, rightOnly = false, radius = dp(16).toFloat()).apply {
            alpha = PANEL_ALPHA
            setStroke(1, Color.parseColor("#2A00E5A0"))
        }
        setPadding(dp(16), dp(14), dp(16), dp(14))
        // header/secondaryRow each have a weight=1 child meant to push
        // content to the far edge -- that only works if *this* row has a
        // definite width to distribute. A plain addView(header) would leave
        // it wrap_content (LinearLayout's default for a VERTICAL parent),
        // where a weighted child has no extra space to claim.
        addView(header, LinearLayout.LayoutParams(MATCH, WRAP))
        addView(divider, LinearLayout.LayoutParams(MATCH, dp(1)).apply { topMargin = dp(8); bottomMargin = dp(10) })
        addView(statusRow)
        addView(space(4))
        addView(streamInfoText)
        addView(space(8))
        addView(heroRow)
        addView(space(8))
        addView(secondaryRow, LinearLayout.LayoutParams(MATCH, WRAP))
        addView(errorText)
        visibility = View.GONE
    }

    private val container = LinearLayout(context).apply {
        orientation = LinearLayout.HORIZONTAL
        addView(tab, LinearLayout.LayoutParams(dp(30), dp(76)))
        addView(panel, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT))
    }

    private var expanded = false
    private var hasAutoCollapsed = false
    private var pulsing = false
    private val tabPulse = pulseAnimator(tabDot)
    private val statusPulse = pulseAnimator(statusDot)

    init {
        versionText.text = versionName(context)
        root.addView(
            container,
            FrameLayout.LayoutParams(WRAP, WRAP).apply {
                gravity = Gravity.START or Gravity.CENTER_VERTICAL
            },
        )
        tab.setOnClickListener {
            bounce(tab)
            setExpanded(!expanded)
        }
        setExpanded(true, animate = false)
    }

    fun setExpanded(newExpanded: Boolean, animate: Boolean = true) {
        if (expanded == newExpanded) return
        expanded = newExpanded
        val params = panel.layoutParams as LinearLayout.LayoutParams
        val tabTargetAlpha = if (newExpanded) TAB_ALPHA_ACTIVE else TAB_ALPHA_IDLE

        if (!animate) {
            panel.visibility = if (newExpanded) View.VISIBLE else View.GONE
            params.width = if (newExpanded) LinearLayout.LayoutParams.WRAP_CONTENT else 0
            panel.layoutParams = params
            tab.background.alpha = tabTargetAlpha
            return
        }

        panel.visibility = View.VISIBLE
        panel.alpha = if (newExpanded) 0f else 1f
        panel.measure(View.MeasureSpec.UNSPECIFIED, View.MeasureSpec.UNSPECIFIED)
        val targetWidth = panel.measuredWidth
        val startWidth = if (newExpanded) 0 else panel.width
        val endWidth = if (newExpanded) targetWidth else 0
        val tabStartAlpha = tab.background.alpha

        ValueAnimator.ofInt(startWidth, endWidth).apply {
            duration = 260
            // A little overshoot opening (feels like it "pops" into place), a
            // plain decelerate closing (overshooting a collapse just looks
            // like a stutter, nothing there to bounce against).
            interpolator = if (newExpanded) OvershootInterpolator(1.6f) else DecelerateInterpolator()
            addUpdateListener { animator ->
                val value = animator.animatedValue as Int
                // OvershootInterpolator can push the animated int slightly
                // past the target on the way in; clamp so the layout width
                // never goes negative.
                params.width = value.coerceAtLeast(0)
                panel.layoutParams = params
                val fraction = animator.animatedFraction.coerceIn(0f, 1f)
                tab.background.alpha =
                    (tabStartAlpha + (tabTargetAlpha - tabStartAlpha) * fraction).toInt()
                panel.alpha = if (newExpanded) fraction else 1f - fraction
            }
            addListener(
                object : AnimatorListenerAdapter() {
                    override fun onAnimationEnd(animation: Animator) {
                        if (!newExpanded) {
                            panel.visibility = View.GONE
                        } else {
                            params.width = LinearLayout.LayoutParams.WRAP_CONTENT
                            panel.layoutParams = params
                            panel.alpha = 1f
                        }
                        tab.background.alpha = tabTargetAlpha
                    }
                },
            )
            start()
        }
    }

    /** Called on every refresh tick from [DisplayActivity]. */
    fun update(
        phase: Phase,
        streamInfo: String?,
        fps: Double,
        frames: Long,
        lastError: String?,
        staleMs: Long,
    ) {
        val live = frames > 0 && staleMs in 0..3000
        val color = when {
            lastError != null && frames == 0L -> COLOR_ERROR
            frames == 0L -> COLOR_WAITING
            staleMs > 3000 -> COLOR_STALLED
            else -> COLOR_LIVE
        }
        tabDot.background.setTint(color)
        statusDot.background.setTint(color)
        setPulsing(live)

        statusLabel.text = phase.label
        streamInfoText.text = streamInfo ?: "—"
        fpsNumber.text = if (frames > 0) "%.0f".format(fps) else "—"
        framesText.text = "%,d frames".format(frames)
        ageText.text = when {
            frames == 0L -> ""
            staleMs < 1000 -> "live"
            else -> "%.1fs ago".format(staleMs / 1000.0)
        }
        errorText.visibility = if (lastError != null) View.VISIBLE else View.GONE
        errorText.text = lastError

        if (frames > 0 && !hasAutoCollapsed) {
            hasAutoCollapsed = true
            setExpanded(false)
        }
    }

    private fun setPulsing(shouldPulse: Boolean) {
        if (shouldPulse == pulsing) return
        pulsing = shouldPulse
        for ((animator, view) in listOf(tabPulse to tabDot, statusPulse to statusDot)) {
            if (shouldPulse) {
                animator.start()
            } else {
                animator.cancel()
                view.scaleX = 1f
                view.scaleY = 1f
                view.alpha = 1f
            }
        }
    }

    /** A gentle, continuous breathing scale+fade — only running while [pulsing]. */
    private fun pulseAnimator(view: View): ValueAnimator =
        // Two properties driven off one fraction rather than
        // PropertyValuesHolder-per-property, so scale and alpha stay in
        // lockstep without juggling separate animator lifecycles.
        ValueAnimator.ofFloat(0f, 1f).apply {
            duration = 1100
            repeatMode = ValueAnimator.REVERSE
            repeatCount = ValueAnimator.INFINITE
            addUpdateListener {
                val t = it.animatedFraction
                val scale = 1f + 0.35f * t
                view.scaleX = scale
                view.scaleY = scale
                view.alpha = 1f - 0.5f * t
            }
        }

    /** Quick press feedback: scale down and spring back. */
    private fun bounce(view: View) {
        ObjectAnimator.ofFloat(view, "scaleX", 1f, 0.88f, 1f).apply {
            duration = 220
            interpolator = OvershootInterpolator(3f)
            start()
        }
        ObjectAnimator.ofFloat(view, "scaleY", 1f, 0.88f, 1f).apply {
            duration = 220
            interpolator = OvershootInterpolator(3f)
            start()
        }
    }

    /** `versionName@versionCode`, so a rebuilt-and-reinstalled APK is
     * distinguishable on screen from whatever was running before it —
     * useful while iterating without needing adb to check. */
    private fun versionName(context: Context): String = runCatching {
        val info = context.packageManager.getPackageInfo(context.packageName, 0)
        "v${info.versionName}"
    }.getOrDefault("")

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
        // Alpha kept out of the color itself (fully opaque `BG`) and applied
        // separately via Drawable.alpha, so the tab's idle/active alpha can
        // be animated without recreating the drawable.
        val BG = Color.parseColor("#161B22")
        const val TAB_ALPHA_IDLE = 45 // ~18% — present, but stays out of the way
        const val TAB_ALPHA_ACTIVE = 240 // ~94% — clearly a control once touched
        const val PANEL_ALPHA = 240
        val COLOR_ACCENT = Color.parseColor("#00E5A0")
        val COLOR_LIVE = Color.parseColor("#00E5A0")
        val COLOR_STALLED = Color.parseColor("#FFC107")
        val COLOR_WAITING = Color.parseColor("#5C6672")
        val COLOR_ERROR = Color.parseColor("#FF5252")

        const val WRAP = FrameLayout.LayoutParams.WRAP_CONTENT
        const val MATCH = FrameLayout.LayoutParams.MATCH_PARENT
    }
}
