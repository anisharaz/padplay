// SPDX-License-Identifier: Apache-2.0

package com.moreland.display

import android.animation.Animator
import android.animation.AnimatorListenerAdapter
import android.animation.AnimatorSet
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
 * A near-invisible status dot that opens a centered status dialog on tap —
 * an "alert dialog", not a panel: nothing about it is present on screen
 * until asked for.
 *
 * The dot itself is the only thing shown by default: small, low-alpha, static,
 * tinted by connection state — enough to confirm "is this working" at a
 * glance without competing with the picture or drawing the eye with motion.
 * Tapping it dims the screen and centers a card with the full readout
 * (status, FPS, resolution, frame count, last-frame age); tapping the
 * scrim or the close button dismisses it the same way a web alert dialog
 * would. There's no auto-open, auto-collapse, or persistent panel state to
 * track — the dot always reflects the latest [update], whether or not the
 * dialog happens to be open.
 */
// See the same suppression on DisplayActivity: no localization plan for a
// single-purpose diagnostic overlay.
@Suppress("SetTextI18n")
class StatsWidget(private val context: Context, root: FrameLayout) {

    private val density = context.resources.displayMetrics.density
    private fun dp(v: Int) = (v * density).toInt()

    // --- the indicator: a dot and nothing else -----------------------------

    private val indicatorDot = View(context).apply {
        background = GradientDrawable().apply {
            shape = GradientDrawable.OVAL
            setColor(COLOR_WAITING)
        }
        alpha = DOT_ALPHA_IDLE
        layoutParams = FrameLayout.LayoutParams(dp(9), dp(9)).apply { gravity = Gravity.CENTER }
    }

    /** Larger than the visible dot so the tap target isn't a 9dp pinprick;
     * stays fully transparent itself — only [indicatorDot] is ever painted. */
    private val indicatorTouchTarget = FrameLayout(context).apply {
        addView(indicatorDot)
    }

    // --- the dialog: built once, shown/hidden as a whole --------------------

    private val dialogTitle = TextView(context).apply {
        text = "Moreland"
        setTextColor(COLOR_ACCENT)
        textSize = 13f
        typeface = Typeface.create(Typeface.SANS_SERIF, Typeface.BOLD)
        letterSpacing = 0.12f
        layoutParams = LinearLayout.LayoutParams(0, WRAP, 1f)
    }

    private val closeButton = TextView(context).apply {
        text = "✕"
        setTextColor(Color.parseColor("#8A94A0"))
        textSize = 15f
        gravity = Gravity.CENTER
        setPadding(dp(10), dp(10), dp(10), dp(10))
    }

    private val dialogHeader = LinearLayout(context).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
        addView(dialogTitle)
        addView(closeButton)
    }

    private val divider = View(context).apply {
        setBackgroundColor(Color.parseColor("#332B3540"))
    }

    private val dialogDot = View(context).apply {
        background = GradientDrawable().apply {
            shape = GradientDrawable.OVAL
            setColor(COLOR_WAITING)
        }
    }

    private val statusLabel = TextView(context).apply {
        setTextColor(Color.WHITE)
        textSize = 14f
        typeface = Typeface.create(Typeface.SANS_SERIF, Typeface.BOLD)
    }

    private val statusRow = LinearLayout(context).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
        addView(dialogDot, LinearLayout.LayoutParams(dp(9), dp(9)).apply { rightMargin = dp(8) })
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

    private val versionText = TextView(context).apply {
        setTextColor(Color.parseColor("#4A525C"))
        textSize = 10f
        typeface = Typeface.MONOSPACE
        gravity = Gravity.END
    }

    private fun space(heightDp: Int) = View(context).apply {
        layoutParams = LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, dp(heightDp))
    }

    private val dialogCard = LinearLayout(context).apply {
        orientation = LinearLayout.VERTICAL
        background = GradientDrawable().apply {
            shape = GradientDrawable.RECTANGLE
            cornerRadius = dp(18).toFloat()
            setColor(CARD_BG)
            setStroke(dp(1).coerceAtLeast(1), Color.parseColor("#332F3B45"))
        }
        elevation = dp(12).toFloat()
        setPadding(dp(20), dp(18), dp(20), dp(18))
        addView(dialogHeader, LinearLayout.LayoutParams(MATCH, WRAP))
        addView(divider, LinearLayout.LayoutParams(MATCH, dp(1)).apply { topMargin = dp(10); bottomMargin = dp(12) })
        addView(statusRow)
        addView(space(4))
        addView(streamInfoText)
        addView(space(10))
        addView(heroRow)
        addView(space(10))
        addView(secondaryRow, LinearLayout.LayoutParams(MATCH, WRAP))
        addView(errorText)
        addView(space(10))
        addView(versionText, LinearLayout.LayoutParams(MATCH, WRAP))
        layoutParams = FrameLayout.LayoutParams(dp(300), WRAP).apply { gravity = Gravity.CENTER }
    }

    /** Full-screen scrim behind the card; tapping it dismisses, same as
     * tapping outside a web alert dialog. */
    private val scrim = FrameLayout(context).apply {
        setBackgroundColor(SCRIM_COLOR)
        visibility = View.GONE
        alpha = 0f
        addView(dialogCard)
        setOnClickListener { hide() }
    }

    private var visible = false
    private var pulsing = false
    private val dialogPulse = pulseAnimator(dialogDot)

    init {
        versionText.text = versionName(context)
        root.addView(
            indicatorTouchTarget,
            FrameLayout.LayoutParams(dp(44), dp(44)).apply {
                gravity = Gravity.START or Gravity.CENTER_VERTICAL
                leftMargin = dp(14)
            },
        )
        root.addView(scrim, FrameLayout.LayoutParams(MATCH, MATCH))

        indicatorTouchTarget.setOnClickListener { show() }
        // Consume clicks on the card itself so they don't fall through to
        // the scrim's dismiss handler underneath it.
        dialogCard.setOnClickListener { }
        closeButton.setOnClickListener { hide() }
    }

    private fun show() {
        if (visible) return
        visible = true
        scrim.visibility = View.VISIBLE
        dialogCard.scaleX = 0.92f
        dialogCard.scaleY = 0.92f
        dialogCard.alpha = 0f
        AnimatorSet().apply {
            playTogether(
                ObjectAnimator.ofFloat(scrim, "alpha", 0f, 1f).setDuration(160),
                ObjectAnimator.ofFloat(dialogCard, "scaleX", 0.92f, 1f).apply {
                    duration = 200
                    interpolator = OvershootInterpolator(1.8f)
                },
                ObjectAnimator.ofFloat(dialogCard, "scaleY", 0.92f, 1f).apply {
                    duration = 200
                    interpolator = OvershootInterpolator(1.8f)
                },
                ObjectAnimator.ofFloat(dialogCard, "alpha", 0f, 1f).setDuration(160),
            )
            start()
        }
    }

    private fun hide() {
        if (!visible) return
        visible = false
        AnimatorSet().apply {
            playTogether(
                ObjectAnimator.ofFloat(scrim, "alpha", 1f, 0f).setDuration(140),
                ObjectAnimator.ofFloat(dialogCard, "scaleX", 1f, 0.96f).setDuration(140),
                ObjectAnimator.ofFloat(dialogCard, "scaleY", 1f, 0.96f).setDuration(140),
                ObjectAnimator.ofFloat(dialogCard, "alpha", 1f, 0f).setDuration(140),
            )
            interpolator = DecelerateInterpolator()
            addListener(object : AnimatorListenerAdapter() {
                override fun onAnimationEnd(animation: Animator) {
                    scrim.visibility = View.GONE
                }
            })
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
        indicatorDot.background.setTint(color)
        dialogDot.background.setTint(color)
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
    }

    /** Only [dialogDot], inside the opened dialog, ever pulses — the
     * always-visible [indicatorDot] stays static so it stays out of the way. */
    private fun setPulsing(shouldPulse: Boolean) {
        if (shouldPulse == pulsing) return
        pulsing = shouldPulse
        if (shouldPulse) {
            dialogPulse.start()
        } else {
            dialogPulse.cancel()
            dialogDot.scaleX = 1f
            dialogDot.scaleY = 1f
            dialogDot.alpha = 1f
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

    /** `versionName@versionCode`, so a rebuilt-and-reinstalled APK is
     * distinguishable on screen from whatever was running before it —
     * useful while iterating without needing adb to check. */
    private fun versionName(context: Context): String = runCatching {
        val info = context.packageManager.getPackageInfo(context.packageName, 0)
        "v${info.versionName}"
    }.getOrDefault("")

    private companion object {
        val CARD_BG = Color.parseColor("#161B22")
        val SCRIM_COLOR = Color.parseColor("#B3000A0D")
        const val DOT_ALPHA_IDLE = 0.55f
        val COLOR_ACCENT = Color.parseColor("#00E5A0")
        val COLOR_LIVE = Color.parseColor("#00E5A0")
        val COLOR_STALLED = Color.parseColor("#FFC107")
        val COLOR_WAITING = Color.parseColor("#5C6672")
        val COLOR_ERROR = Color.parseColor("#FF5252")

        const val WRAP = FrameLayout.LayoutParams.WRAP_CONTENT
        const val MATCH = FrameLayout.LayoutParams.MATCH_PARENT
    }
}
