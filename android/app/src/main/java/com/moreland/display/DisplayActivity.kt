// SPDX-License-Identifier: Apache-2.0

package com.moreland.display

import android.app.Activity
import android.graphics.Color
import android.os.Bundle
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.WindowManager
import android.widget.FrameLayout
import android.widget.TextView

/**
 * Fullscreen host for the decoded stream.
 *
 * The stream is tied to the surface lifecycle rather than the activity's:
 * a surface can be destroyed and recreated without the activity restarting,
 * and decoding into a dead surface is a crash.
 */
class DisplayActivity : Activity(), SurfaceHolder.Callback {

    private lateinit var surfaceView: SurfaceView
    private lateinit var status: TextView
    private lateinit var overlay: TextView
    private var stream: VideoStream? = null
    private var lastFramesDecoded = 0L
    private var lastFpsSampleAtMs = 0L

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        window.setBackgroundDrawableResource(android.R.color.black)

        val root = FrameLayout(this).apply { setBackgroundColor(Color.BLACK) }

        status = TextView(this).apply {
            text = "Waiting for host…\n\nConnect the tablet and start the daemon."
            setTextColor(Color.parseColor("#888888"))
            textSize = 16f
            val pad = (24 * resources.displayMetrics.density).toInt()
            setPadding(pad, pad, pad, pad)
        }
        // Small persistent corner readout, unlike `status`: it never fully
        // disappears once streaming starts, because "was live a minute ago"
        // and "is live right now" need to look different on screen — a
        // stalled stream with the last frame frozen on screen is otherwise
        // indistinguishable from a healthy one.
        overlay = TextView(this).apply {
            setTextColor(Color.parseColor("#CCFFFFFF"))
            setBackgroundColor(Color.parseColor("#66000000"))
            textSize = 12f
            typeface = android.graphics.Typeface.MONOSPACE
            val pad = (8 * resources.displayMetrics.density).toInt()
            setPadding(pad, pad, pad, pad)
        }
        surfaceView = SurfaceView(this)

        root.addView(surfaceView, FrameLayout.LayoutParams(MATCH, MATCH))
        root.addView(status, FrameLayout.LayoutParams(MATCH, WRAP))
        root.addView(
            overlay,
            FrameLayout.LayoutParams(WRAP, WRAP).apply {
                gravity = android.view.Gravity.TOP or android.view.Gravity.START
            },
        )
        setContentView(root)

        stream = VideoStream().also { it.start() }
        surfaceView.holder.addCallback(this)
        goImmersive()
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) goImmersive()
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

    override fun surfaceCreated(holder: SurfaceHolder) {
        // The stream outlives individual surfaces. Only the render target is
        // swapped here — creating a stream per surface races two instances for
        // the single process-wide socket name.
        stream?.setSurface(holder.surface)
        status.post(::refreshStatus)
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        stream?.setSurface(holder.surface)
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        stream?.setSurface(null)
    }

    override fun onDestroy() {
        stream?.stop()
        stream = null
        super.onDestroy()
    }

    private fun refreshStatus() {
        val stream = this.stream ?: return

        // The big centered message only makes sense before anything has ever
        // rendered — once a frame is on screen it would just sit on top of
        // the picture. `overlay` takes over as the sole readout from then on.
        status.visibility = if (stream.framesDecoded > 0) View.GONE else View.VISIBLE
        if (stream.framesDecoded == 0L) {
            status.text = buildString {
                append(stream.state)
                stream.lastError?.let { append("\n\nLast error: $it") }
            }
        }

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

        val staleness = if (stream.lastFrameAtMs > 0) now - stream.lastFrameAtMs else -1L
        val liveness = when {
            stream.framesDecoded == 0L -> ""
            staleness > 3000 -> "  ⚠ STALLED ${staleness / 1000}s"
            else -> "  ● LIVE"
        }

        overlay.text = buildString {
            append(stream.state).append(liveness).append('\n')
            stream.streamInfo?.let { append(it).append("  ") }
            append("%.1f fps".format(fps)).append("  ")
            append(stream.framesDecoded).append(" frames")
            stream.lastError?.let { append("\nlast error: ").append(it) }
        }

        overlay.postDelayed(::refreshStatus, 500)
    }

    private companion object {
        const val MATCH = FrameLayout.LayoutParams.MATCH_PARENT
        const val WRAP = FrameLayout.LayoutParams.WRAP_CONTENT
    }
}
