package dev.tinyrtc.demo

import android.app.Activity
import android.graphics.Color
import android.hardware.camera2.CameraManager
import android.os.Handler
import android.os.Looper
import android.widget.LinearLayout
import dev.tinyrtc.CameraOptions
import dev.tinyrtc.DataChannelOptions
import dev.tinyrtc.LocalVideoTrack
import dev.tinyrtc.MediaKind
import dev.tinyrtc.RtcDevice
import dev.tinyrtc.RtcEngine
import dev.tinyrtc.RtcError
import dev.tinyrtc.RtcVideoView
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import org.json.JSONArray
import org.json.JSONObject

/**
 * The camera track's own checks, with no peer and no tapping:
 *
 *   adb shell am start -n dev.tinyrtc.demo/.MainActivity -e check camera [--ei views 3] [-e applyCamera <id>]
 *
 * One JSON line per step under `tinyrtc-demo` (event "check", `ok` true or false), the last one
 * `{"event":"check","step":"done"}`:
 * - cameras: the ids `applyCamera` takes.
 * - cancel: openCamera cancelled after 0 to 1000 ms, seven times; then no "tinyrtc-video" thread is
 *   alive and no camera is held (the availability callback's word: another app's camera counts too).
 * - views: the track in `views` views at once (default 2): the frames each took. The first ones up to
 *   the camera's limit take frames, those past it none (and the `tinyrtc` log says so); then again
 *   once the first view let go.
 * - apply-unknown: a camera id that does not exist: NotFound, and the track goes on as it was.
 * - apply: to `applyCamera` when given: it runs on that camera, or (it failed) on the one before.
 * - errors: the preconditions throw RtcError.Failed.
 * - stopped: after stop(), no thread and no camera left.
 */
internal class CameraCheck(private val activity: Activity, private val log: (JSONObject) -> Unit) {
    private val cameras = activity.getSystemService(CameraManager::class.java)
    private val free = HashMap<String, Boolean>()
    private val watch = object : CameraManager.AvailabilityCallback() {
        override fun onCameraAvailable(cameraId: String) {
            free[cameraId] = true
        }

        override fun onCameraUnavailable(cameraId: String) {
            free[cameraId] = false
        }
    }

    suspend fun run(views: Int, applyCamera: String?) {
        val engine = RtcEngine.getInstance(activity)
        cameras.registerAvailabilityCallback(watch, Handler(Looper.getMainLooper()))
        try {
            val ids = engine.devices().filter { it.kind == RtcDevice.Kind.CAMERA }.map { "${it.id} ${it.facing}" }
            report("cameras", ids.isNotEmpty(), JSONObject().put("cameras", JSONArray(ids)))
            cancelled(engine)
            val track = engine.openCamera()
            try {
                shown(track, views)
                applied(track, applyCamera)
                preconditions(engine, track)
            } finally {
                track.stop()
            }
            delay(2500)
            leftovers("stopped")
        } catch (e: Throwable) {
            if (e is kotlinx.coroutines.CancellationException) throw e
            report("error", false, JSONObject().put("error", e.toString()))
        } finally {
            cameras.unregisterAvailabilityCallback(watch)
            log(JSONObject().put("event", "check").put("step", "done"))
        }
    }

    private suspend fun cancelled(engine: RtcEngine) = coroutineScope {
        val rounds = JSONArray()
        for (ms in listOf(0L, 20, 60, 150, 300, 600, 1000)) {
            var opened: LocalVideoTrack? = null
            var error: Throwable? = null
            val open = launch {
                try {
                    opened = engine.openCamera()
                } catch (e: RtcError) {
                    error = e
                }
            }
            delay(ms)
            open.cancel()
            open.join()
            rounds.put(JSONObject().put("ms", ms).put("opened", opened != null).put("error", error?.toString()))
            opened?.stop()
            delay(300)
        }
        delay(2500) // a closing camera says onClosed well within this
        leftovers("cancel", JSONObject().put("rounds", rounds))
    }

    private suspend fun shown(track: LocalVideoTrack, count: Int) {
        val column = LinearLayout(activity).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(Color.BLACK)
        }
        val shown = List(count.coerceIn(1, 6)) { RtcVideoView(activity).apply { objectFit = RtcVideoView.ObjectFit.CONTAIN } }
        shown.forEach { column.addView(it, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f)) }
        activity.setContentView(column)
        shown.forEach { it.track = track }
        delay(3000)
        val frames = shown.map { it.framesShown }
        // The views that show the camera are the first ones: no view takes frames after a black one.
        val prefix = frames.indexOfFirst { it == 0L }.let { black -> black < 0 || frames.drop(black).all { it == 0L } }
        report("views", frames[0] > 0 && prefix, JSONObject().put("frames", JSONArray(frames)))
        if (shown.size > 1) {
            shown[0].track = null
            val before = shown.map { it.framesShown }
            delay(2500)
            val gained = shown.indices.map { shown[it].framesShown - before[it] }
            report("views-after-first-left", gained[1] > 0, JSONObject().put("gained", JSONArray(gained)))
        }
        shown.forEach { it.track = null }
    }

    private suspend fun applied(track: LocalVideoTrack, camera: String?) {
        val before = track.settings
        val unknown = runCatching { track.apply(CameraOptions(deviceId = "no-such-camera")) }.exceptionOrNull()
        report("apply-unknown", unknown is RtcError.NotFound && track.settings == before && runs(track),
            JSONObject().put("error", unknown?.toString()))
        if (camera == null) return
        val failure = runCatching { track.apply(CameraOptions(deviceId = camera)) }.exceptionOrNull()
        val on = if (failure == null) track.settings.deviceId == camera else track.settings == before
        report("apply", !track.isStopped && on && runs(track),
            JSONObject().put("error", failure?.toString()).put("settings", track.settings.toString()))
    }

    private suspend fun preconditions(engine: RtcEngine, track: LocalVideoTrack) {
        val connection = engine.connect()
        try {
            val failed = JSONObject()
            failed.put("setTrack", failsWith { connection.setTrack(MediaKind.AUDIO, track) })
            failed.put("createDataChannel", failsWith {
                connection.createDataChannel("both", DataChannelOptions(maxRetransmits = 1, maxPacketLifeTimeMs = 100))
            })
            failed.put("setMaxBitrate", failsWith { track.setMaxBitrate(0) })
            val channel = connection.createDataChannel("check")
            failed.put("bufferedAmountLowThreshold", failsWith { channel.bufferedAmountLowThreshold = -1 })
            channel.close()
            report("errors", failed.keys().asSequence().all { failed.getString(it) == "Failed" }, failed)
        } finally {
            connection.close()
        }
    }

    /** "Failed" when [block] threw RtcError.Failed; else what it did. */
    private suspend fun failsWith(block: suspend () -> Unit): String = try {
        block()
        "nothing"
    } catch (e: RtcError.Failed) {
        "Failed"
    } catch (e: Exception) {
        if (e is kotlinx.coroutines.CancellationException) throw e
        e.javaClass.simpleName
    }

    /** Frames keep coming out of the encoder. */
    private suspend fun runs(track: LocalVideoTrack): Boolean {
        val encoded = track.diagnostics.framesEncoded
        delay(1500)
        return track.diagnostics.framesEncoded > encoded
    }

    private fun leftovers(step: String, details: JSONObject = JSONObject()) {
        val threads = Thread.getAllStackTraces().keys.count { it.isAlive && it.name == "tinyrtc-video" }
        val held = free.filterValues { !it }.keys
        report(step, threads == 0 && held.isEmpty(), details.put("videoThreads", threads).put("camerasHeld", JSONArray(held)))
    }

    private fun report(step: String, ok: Boolean, details: JSONObject) {
        log(details.put("event", "check").put("step", step).put("ok", ok))
    }
}
