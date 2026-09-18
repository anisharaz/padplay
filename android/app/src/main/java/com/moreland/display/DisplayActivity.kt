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

/**
 * Fullscreen host for the decoded stream.
 *
 * The stream is tied to the surface lifecycle rather than the activity's:
 * a surface can be destroyed and recreated without the activity restarting,
 * and decoding into a dead surface is a crash.
 */
class DisplayActivity : Activity(), SurfaceHolder.Callback {

    private lateinit var surfaceView: SurfaceView
    private lateinit var stats: StatsWidget
    private var stream: VideoStream? = null
    private var lastFramesDecoded = 0L
    private var lastFpsSampleAtMs = 0L

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        window.setBackgroundDrawableResource(android.R.color.black)

        val root = FrameLayout(this).apply { setBackgroundColor(Color.BLACK) }
        surfaceView = SurfaceView(this)
        root.addView(surfaceView, FrameLayout.LayoutParams(MATCH, MATCH))
        stats = StatsWidget(this, root)
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
        surfaceView.post(::refreshStats)
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

    private fun refreshStats() {
        val stream = this.stream ?: return

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
            state = stream.state,
            streamInfo = stream.streamInfo,
            fps = fps,
            frames = stream.framesDecoded,
            lastError = stream.lastError,
            staleMs = staleMs,
        )

        surfaceView.postDelayed(::refreshStats, 500)
    }

    private companion object {
        const val MATCH = FrameLayout.LayoutParams.MATCH_PARENT
    }
}
