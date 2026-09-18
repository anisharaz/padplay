// SPDX-License-Identifier: Apache-2.0

package com.moreland.display

import android.app.Activity
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.os.Bundle
import android.view.Gravity
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.WindowManager
import android.widget.CompoundButton
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.Switch
import android.widget.TextView

/**
 * Fullscreen host for the decoded stream, switching between two views:
 *
 * - **Home** (Normal mode): a plain, non-immersive screen with branding, an
 *   "accept connections" toggle, and current status. What you see with no
 *   host attached, or with accepting turned off.
 * - **Display**: fullscreen immersive video + [StatsWidget]. Entered
 *   automatically as soon as a host connects, left automatically as soon as
 *   it disconnects.
 *
 * [VideoStream] outlives both: toggling "accept connections" calls its
 * start/stop, but the object itself (and its process-wide socket name) is
 * created once and kept for the activity's life. The surface it renders into
 * is a property of the stream for the same reason it always was — Display's
 * `SurfaceView` can be destroyed and recreated (immersive mode flicker,
 * rotation) without tearing down an in-progress decode.
 */
class DisplayActivity : Activity(), SurfaceHolder.Callback {

    private enum class Mode { HOME, DISPLAY }

    private lateinit var homeView: View
    private lateinit var displayView: View
    private lateinit var surfaceView: SurfaceView
    private lateinit var stats: StatsWidget
    private lateinit var acceptSwitch: Switch
    private lateinit var homeStatus: TextView

    private val stream = VideoStream()
    private var mode = Mode.HOME
    private var lastFramesDecoded = 0L
    private var lastFpsSampleAtMs = 0L
    private var tickStarted = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        window.setBackgroundDrawableResource(android.R.color.black)

        val root = FrameLayout(this).apply { setBackgroundColor(Color.parseColor("#0D1117")) }

        // displayView (and the SurfaceView inside it) is added first and its
        // own visibility is never touched after this — a View.GONE
        // SurfaceView never gets a Surface at all, so toggling it to hide
        // Display mode meant awaitSurface() in VideoStream just timed out
        // the moment a host actually connected. Home mode is instead a
        // separate, fully opaque view stacked *on top*, shown/hidden on its
        // own — the SurfaceView underneath stays alive and surfaced the
        // entire time regardless of which mode is visible.
        displayView = FrameLayout(this).apply {
            setBackgroundColor(Color.BLACK)
            surfaceView = SurfaceView(this@DisplayActivity)
            addView(surfaceView, FrameLayout.LayoutParams(MATCH, MATCH))
        }
        stats = StatsWidget(this, displayView as FrameLayout)
        root.addView(displayView, FrameLayout.LayoutParams(MATCH, MATCH))

        homeView = buildHomeView()
        root.addView(homeView, FrameLayout.LayoutParams(MATCH, MATCH))

        setContentView(root)

        surfaceView.holder.addCallback(this)
        setMode(Mode.HOME)

        // Matches the app's previous always-on behavior by default; the
        // switch is what makes that a choice now instead of a given.
        acceptSwitch.isChecked = true
    }

    private fun buildHomeView(): View {
        val dp = { v: Int -> (v * resources.displayMetrics.density).toInt() }

        val title = TextView(this).apply {
            text = "Moreland"
            setTextColor(Color.parseColor("#00E5A0"))
            textSize = 28f
            typeface = Typeface.create(Typeface.DEFAULT, Typeface.BOLD)
        }
        val subtitle = TextView(this).apply {
            text = "This tablet is ready to be a second monitor."
            setTextColor(Color.parseColor("#AAAAAA"))
            textSize = 15f
            setPadding(0, dp(8), 0, dp(28))
        }

        val switchRow = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            background = GradientDrawable().apply {
                setColor(Color.parseColor("#1A1F26"))
                cornerRadius = dp(14).toFloat()
            }
            setPadding(dp(18), dp(14), dp(18), dp(14))

            val label = TextView(this@DisplayActivity).apply {
                text = "Accept connections"
                setTextColor(Color.WHITE)
                textSize = 16f
                layoutParams = LinearLayout.LayoutParams(0, WRAP, 1f)
            }
            acceptSwitch = Switch(this@DisplayActivity).apply {
                setOnCheckedChangeListener { _: CompoundButton, checked: Boolean ->
                    if (checked) stream.start() else stream.stop()
                    refresh()
                }
            }
            addView(label)
            addView(acceptSwitch)
        }

        homeStatus = TextView(this).apply {
            setTextColor(Color.parseColor("#888888"))
            textSize = 13f
            typeface = Typeface.MONOSPACE
            setPadding(0, dp(20), 0, 0)
        }

        val column = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER
            addView(title)
            addView(subtitle)
            addView(switchRow, LinearLayout.LayoutParams(dp(320), WRAP))
            addView(homeStatus)
        }

        return FrameLayout(this).apply {
            // Opaque: this sits directly on top of displayView's SurfaceView
            // in the stack, and needs to fully hide it while shown.
            setBackgroundColor(Color.parseColor("#0D1117"))
            addView(column, FrameLayout.LayoutParams(WRAP, WRAP, Gravity.CENTER))
        }
    }

    /** Only ever touches `homeView`'s visibility — see the comment on
     * `displayView`'s construction in [onCreate] for why. */
    private fun setMode(newMode: Mode) {
        mode = newMode
        homeView.visibility = if (newMode == Mode.HOME) View.VISIBLE else View.GONE
        if (newMode == Mode.DISPLAY) goImmersive() else exitImmersive()
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus && mode == Mode.DISPLAY) goImmersive()
    }

    @Suppress("DEPRECATION")
    private fun goImmersive() {
        window.decorView.systemUiVisibility = (
            View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                or View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                or View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                or View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                or View.SYSTEM_UI_FLAG_FULLSCREEN
                or View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
            )
    }

    @Suppress("DEPRECATION")
    private fun exitImmersive() {
        window.decorView.systemUiVisibility = View.SYSTEM_UI_FLAG_LAYOUT_STABLE
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        // The stream outlives individual surfaces. Only the render target is
        // swapped here — creating a stream per surface races two instances for
        // the single process-wide socket name.
        stream.setSurface(holder.surface)
        // Surfaces churn far more than this fires once, though (immersive
        // mode flicker recreates them on every touch) -- guard so repeat
        // calls don't stack up parallel tick loops.
        if (!tickStarted) {
            tickStarted = true
            surfaceView.post(::tick)
        }
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        stream.setSurface(holder.surface)
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        stream.setSurface(null)
    }

    override fun onDestroy() {
        stream.stop()
        super.onDestroy()
    }

    /**
     * Recomputes mode/stats from [stream]'s current state. Pure — doesn't
     * schedule anything, so it's safe to call directly for immediate
     * feedback (e.g. the accept switch) without risking a second parallel
     * [tick] loop alongside the one [surfaceCreated] already started.
     */
    private fun refresh() {
        setMode(if (stream.phase.isActive) Mode.DISPLAY else Mode.HOME)

        val now = System.currentTimeMillis()
        val fps = if (lastFpsSampleAtMs > 0) {
            val elapsedS = (now - lastFpsSampleAtMs) / 1000.0
            val delta = stream.framesDecoded - lastFramesDecoded
            if (elapsedS > 0) delta / elapsedS else 0.0
        } else {
            0.0
        }
        lastFramesDecoded = stream.framesDecoded
        lastFpsSampleAtMs = now

        val staleMs = if (stream.lastFrameAtMs > 0) now - stream.lastFrameAtMs else -1L
        stats.update(
            phase = stream.phase,
            streamInfo = stream.streamInfo,
            fps = fps,
            frames = stream.framesDecoded,
            lastError = stream.lastError,
            staleMs = staleMs,
        )

        homeStatus.text = when {
            !stream.isRunning -> "Not accepting connections."
            else -> stream.phase.label + (stream.lastError?.let { "\nlast: $it" } ?: "")
        }
    }

    /** The one continuously-rescheduling loop; started once from [surfaceCreated]. */
    private fun tick() {
        refresh()
        surfaceView.postDelayed(::tick, 500)
    }

    private companion object {
        const val MATCH = FrameLayout.LayoutParams.MATCH_PARENT
        const val WRAP = FrameLayout.LayoutParams.WRAP_CONTENT
    }
}
