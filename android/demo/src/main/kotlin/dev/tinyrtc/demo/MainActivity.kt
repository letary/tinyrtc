package dev.tinyrtc.demo

import android.app.Activity
import android.graphics.Color
import android.os.Build
import android.os.Bundle
import android.util.Log
import android.view.WindowManager
import android.widget.LinearLayout
import dev.tinyrtc.CameraOptions
import dev.tinyrtc.LocalAudioTrack
import dev.tinyrtc.LocalVideoTrack
import dev.tinyrtc.MediaKind
import dev.tinyrtc.RtcConnection
import dev.tinyrtc.RtcDataChannel
import dev.tinyrtc.RtcEngine
import dev.tinyrtc.RtcVideoView
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.MainScope
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.json.JSONArray
import org.json.JSONObject

/**
 * The Android half against tools/peer, with no tapping:
 *
 *   adb shell am start -n dev.tinyrtc.demo/.MainActivity -e peer http://<pc>:<port> [--ei seconds 20]
 *
 * Opens the camera and the microphone (permissions granted beforehand with `pm grant`), offers to
 * the peer over HTTP (POST /offer), sends both tracks and a data channel tick per second, shows the
 * echoed video above the local preview, and logs one JSON line per second under `tinyrtc-demo`.
 * `-e check camera` runs the camera track's own checks instead, with no peer (CameraCheck).
 */
class MainActivity : Activity() {
    private val scope = MainScope()
    private lateinit var localView: RtcVideoView
    private lateinit var remoteView: RtcVideoView
    private var connection: RtcConnection? = null
    private var video: LocalVideoTrack? = null
    private var audio: LocalAudioTrack? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        remoteView = RtcVideoView(this).apply { objectFit = RtcVideoView.ObjectFit.CONTAIN }
        localView = RtcVideoView(this).apply {
            objectFit = RtcVideoView.ObjectFit.CONTAIN
            mirror = true
        }
        setContentView(LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(Color.BLACK)
            addView(remoteView, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f))
            addView(localView, LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, 0, 1f))
        })

        if (intent.getStringExtra("check") == "camera") {
            // The camera track's own checks, no peer (CameraCheck).
            val views = intent.getStringExtra("views")?.toIntOrNull() ?: intent.getIntExtra("views", 2)
            val applyCamera = intent.getStringExtra("applyCamera")
            log(event("start").put("check", "camera").put("views", views).put("applyCamera", applyCamera)
                .put("device", "${Build.MANUFACTURER} ${Build.MODEL}").put("sdk", Build.VERSION.SDK_INT))
            scope.launch { CameraCheck(this@MainActivity, ::log).run(views, applyCamera) }
            return
        }

        val peer = intent.getStringExtra("peer")
        val seconds = intent.getStringExtra("seconds")?.toIntOrNull() ?: intent.getIntExtra("seconds", 30)
        log(event("start").put("peer", peer).put("seconds", seconds).put("device", "${Build.MANUFACTURER} ${Build.MODEL}")
            .put("sdk", Build.VERSION.SDK_INT))
        if (peer == null) {
            log(event("error").put("error", "no `peer` extra"))
            return
        }
        scope.launch { call(peer.trimEnd('/'), seconds) }
    }

    override fun onDestroy() {
        scope.cancel()
        hangUp()
        super.onDestroy()
    }

    private suspend fun call(peer: String, seconds: Int) {
        try {
            val engine = RtcEngine.getInstance(this)
            val video = engine.openCamera(CameraOptions()).also { this.video = it }
            val audio = engine.openMicrophone().also { this.audio = it }
            localView.track = video

            val connection = engine.connect().also { this.connection = it }
            connection.onState = { log(event("state").put("state", it.name)) }
            connection.onRemoteMedia = { kind, active -> log(event("remoteMedia").put("kind", kind.name).put("active", active)) }
            remoteView.track = connection.remoteVideo
            remoteView.onResize = { w, h -> log(event("remoteResize").put("width", w).put("height", h)) }
            connection.setTrack(MediaKind.AUDIO, audio)
            connection.setTrack(MediaKind.VIDEO, video)

            val channel = connection.createDataChannel("demo")
            var echoes = 0
            channel.onOpen = { log(event("channelOpen").put("label", channel.label)) }
            channel.onMessage = { echoes++ }

            val offer = connection.createOffer()
            val answer = withContext(Dispatchers.IO) { post("$peer/offer", offer) }
            connection.acceptAnswer(answer)
            log(event("signaled").put("offerBytes", offer.length).put("answerBytes", answer.length)
                .put("candidates", JSONArray(offer.lines().filter { it.startsWith("a=candidate") })))

            var sent = 0
            for (t in 1..seconds) {
                delay(1000)
                if (channel.state == RtcDataChannel.State.OPEN) {
                    channel.send("tick $t")
                    sent++
                }
                log(report(t, engine, connection, video, channel, sent, echoes))
            }
            log(event("done"))
        } catch (e: Throwable) {
            if (e is kotlinx.coroutines.CancellationException) throw e
            log(event("error").put("error", e.toString()))
        } finally {
            hangUp()
        }
    }

    private fun hangUp() {
        connection?.close()
        video?.stop()
        audio?.stop()
        connection = null
        video = null
        audio = null
        localView.track = null
        remoteView.track = null
    }

    private suspend fun report(
        t: Int, engine: RtcEngine, connection: RtcConnection, video: LocalVideoTrack, channel: RtcDataChannel,
        sent: Int, echoes: Int,
    ): JSONObject {
        val stats = runCatching { connection.stats() }.getOrNull()
        val v = video.diagnostics
        val r = remoteView.diagnostics
        val a = engine.audioDiagnostics
        return JSONObject()
            .put("t", t)
            .put("state", connection.state.name)
            .put("video", JSONObject()
                .put("encoded", v.framesEncoded).put("sent", v.framesSent).put("keyframes", v.keyframes)
                .put("keyframesRequested", v.keyframesRequested).put("encoder", v.encoder)
                .put("size", "${v.encodeWidth}x${v.encodeHeight}").put("bitrate", v.bitrate).put("rotation", v.rotation)
                .put("paused", v.paused)
                .put("target", JSONObject().put("bps", v.targetBitrate).put("fps", v.targetFps).put("height", v.targetHeight)))
            .put("remote", JSONObject()
                .put("received", r.framesReceived).put("decoded", r.framesDecoded).put("rendered", r.framesRendered)
                .put("shown", remoteView.framesShown).put("dropped", r.framesDropped).put("errors", r.decoderErrors)
                .put("keyframesAsked", r.keyframesRequested).put("width", r.width).put("height", r.height)
                .put("rotation", r.rotation).put("decoder", r.decoder))
            .put("local", JSONObject().put("shown", localView.framesShown)
                .put("width", localView.videoWidth).put("height", localView.videoHeight))
            .put("audio", JSONObject()
                .put("pushed", a.framesPushed).put("pulled", a.framesPulled).put("capturing", a.capturing)
                .put("playing", a.playing).put("effects", JSONArray(a.effects)))
            .put("channel", JSONObject().put("state", channel.state.name).put("sent", sent).put("echoes", echoes))
            .put("stats", stats?.let { s ->
                JSONObject()
                    .put("state", s.state.name).put("relayed", s.relayed).put("rttMs", s.rttMs)
                    .put("sendEstimateBps", s.sendEstimateBps)
                    .put("audioSendBps", s.audioSendBps).put("audioRecvBps", s.audioRecvBps)
                    .put("videoSendBps", s.videoSendBps).put("videoRecvBps", s.videoRecvBps)
                    .put("audioLoss", s.audioLoss.toDouble()).put("videoLoss", s.videoLoss.toDouble())
                    .put("audioJitterBufferMs", s.audioJitterBufferMs).put("videoRecvFps", s.videoRecvFps)
                    .put("videoNacksSent", s.videoNacksSent).put("videoKeyframesRequested", s.videoKeyframesRequested)
                    .put("bytesSent", s.bytesSent).put("bytesReceived", s.bytesReceived)
            } ?: JSONObject.NULL)
    }

    /** The peer's signaling: POST the offer, the answer comes back. */
    private fun post(url: String, body: String): String {
        val http = URL(url).openConnection() as HttpURLConnection
        try {
            http.requestMethod = "POST"
            http.doOutput = true
            http.connectTimeout = 5000
            http.readTimeout = 10000
            http.setRequestProperty("Content-Type", "application/sdp")
            http.outputStream.use { it.write(body.toByteArray()) }
            val code = http.responseCode
            val text = (if (code in 200..299) http.inputStream else http.errorStream)?.bufferedReader()?.use { it.readText() }.orEmpty()
            if (code !in 200..299) throw IOException("POST $url: $code $text")
            return text
        } finally {
            http.disconnect()
        }
    }

    private fun event(name: String) = JSONObject().put("event", name)

    private fun log(json: JSONObject) {
        Log.i(TAG, json.toString())
    }

    private companion object {
        const val TAG = "tinyrtc-demo"
    }
}
