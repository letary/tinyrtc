// tinyrtc_jni.c — dev.tinyrtc.Native over core/include/tinyrtc.h, and nothing more: no decision is
// made here. The natives are registered in JNI_OnLoad (the library's only exported symbol).
//
// Handles cross as jlong. Text crosses as UTF-8 in byte arrays, both ways (a Java String's
// "modified UTF-8" is not UTF-8); a NUL is added here on the way in.
//
// Callbacks. The `user` of a connection or a video source is not a pointer: it is the integer key
// the Kotlin side registered the object under. A callback that comes after the object went (a
// source that a connection still holds after tinyrtc_video_source_destroy keeps calling back)
// finds nothing under its key and is dropped there — nothing here is allocated per object, so
// nothing can be used after it was freed.
//
// Core threads. A callback attaches its thread to the VM the first time it runs on it, keeping
// the thread's name, and a pthread key detaches it when the thread exits. A thread the VM already
// knows (a Java thread inside tinyrtc_connection_send_video, whose source calls back on the
// caller's thread) is used as it is and never detached here. Everything an event points at is
// copied into Java arrays before the dispatcher is called: the pointers die when the callback
// returns. The dispatchers only copy and hop; they never call back into the core.
#include <jni.h>
#include <pthread.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <android/log.h>

#include "tinyrtc.h"

#define TAG "tinyrtc"
#define CONTEXT(h) ((tinyrtc_context *)(intptr_t)(h))
#define SOURCE(h) ((tinyrtc_video_source *)(intptr_t)(h))
#define CONNECTION(h) ((tinyrtc_connection *)(intptr_t)(h))
#define KEY(user) ((jint)(intptr_t)(user))

static JavaVM *vm;
static jclass native_class;
static jmethodID on_connection_event, on_source_target, on_source_keyframe;
static pthread_key_t detach_key;

// ---- threads --------------------------------------------------------------------------------------

static void detach_thread(void *unused) {
  (void)unused;
  (*vm)->DetachCurrentThread(vm);
}

static JNIEnv *thread_env(void) {
  JNIEnv *env = NULL;
  jint got = (*vm)->GetEnv(vm, (void **)&env, JNI_VERSION_1_6);
  if (got == JNI_OK) return env;
  if (got != JNI_EDETACHED) return NULL;
  char name[17] = {0};  // the core names its threads; the VM would rename them otherwise
  prctl(PR_GET_NAME, name, 0, 0, 0);
  JavaVMAttachArgs args = {JNI_VERSION_1_6, name[0] ? name : "tinyrtc", NULL};
  if ((*vm)->AttachCurrentThread(vm, &env, &args) != JNI_OK) return NULL;
  pthread_setspecific(detach_key, env);  // non-NULL: detach_thread runs when the thread exits
  return env;
}

static void clear_exception(JNIEnv *env) {
  if ((*env)->ExceptionCheck(env)) {
    (*env)->ExceptionDescribe(env);
    (*env)->ExceptionClear(env);
  }
}

// ---- copies -----------------------------------------------------------------------------------------

static jbyteArray new_bytes(JNIEnv *env, const void *data, size_t len) {
  if (len > INT32_MAX) return NULL;
  jbyteArray array = (*env)->NewByteArray(env, (jsize)len);
  if (!array) {
    clear_exception(env);
    return NULL;
  }
  if (len) (*env)->SetByteArrayRegion(env, array, 0, (jsize)len, (const jbyte *)data);
  return array;
}

static jbyteArray text_bytes(JNIEnv *env, const char *text) {
  return text ? new_bytes(env, text, strlen(text)) : NULL;
}

// A malloc'd NUL-terminated copy of a byte array; NULL for NULL.
static char *c_text(JNIEnv *env, jbyteArray array) {
  if (!array) return NULL;
  jsize len = (*env)->GetArrayLength(env, array);
  char *text = malloc((size_t)len + 1);
  if (!text) return NULL;
  (*env)->GetByteArrayRegion(env, array, 0, len, (jbyte *)text);
  text[len] = 0;
  return text;
}

static char *c_text_at(JNIEnv *env, jobjectArray array, jsize i) {
  if (!array || i >= (*env)->GetArrayLength(env, array)) return NULL;
  jbyteArray item = (jbyteArray)(*env)->GetObjectArrayElement(env, array, i);
  char *text = c_text(env, item);
  if (item) (*env)->DeleteLocalRef(env, item);
  return text;
}

// The core's text as bytes, and the core's text freed.
static jbyteArray take_text(JNIEnv *env, tinyrtc_text text, jint status) {
  jbyteArray bytes = (status == TINYRTC_OK && text.data) ? new_bytes(env, text.data, text.len) : NULL;
  tinyrtc_text_free(text);
  return bytes;
}

static void set_status(JNIEnv *env, jintArray out, jint status) {
  if (out && (*env)->GetArrayLength(env, out) > 0) (*env)->SetIntArrayRegion(env, out, 0, 1, &status);
}

// ---- callbacks (core threads) -----------------------------------------------------------------------

static void on_log(void *user, tinyrtc_log_level level, const char *line) {
  (void)user;
  int priority = level == TINYRTC_LOG_ERROR  ? ANDROID_LOG_ERROR
                 : level == TINYRTC_LOG_WARN ? ANDROID_LOG_WARN
                 : level == TINYRTC_LOG_INFO ? ANDROID_LOG_INFO
                                             : ANDROID_LOG_DEBUG;
  __android_log_write(priority, TAG, line);
}

static void on_event(void *user, const tinyrtc_event *event) {
  JNIEnv *env = thread_env();
  if (!env) return;
  jint a = 0, b = 0;
  jlong c = 0;
  jbyteArray bytes = NULL;
  switch (event->type) {
    case TINYRTC_EVENT_STATE:
      a = (jint)event->state;
      break;
    case TINYRTC_EVENT_LOCAL_CANDIDATE:
      bytes = text_bytes(env, event->candidate.candidate);
      break;
    case TINYRTC_EVENT_ICE_RESTART_NEEDED:
      break;
    case TINYRTC_EVENT_REMOTE_MEDIA:
      a = (jint)event->remote_media.media;
      b = event->remote_media.active;
      break;
    case TINYRTC_EVENT_VIDEO_FRAME: {
      // One access unit for MediaCodec: Annex B, a 4-byte start code before every NAL unit.
      static const jbyte start[4] = {0, 0, 0, 1};
      size_t total = 0;
      for (size_t i = 0; i < event->video.nal_count; i++) total += sizeof start + event->video.nals[i].len;
      if (total == 0 || total > INT32_MAX) return;
      bytes = (*env)->NewByteArray(env, (jsize)total);
      if (!bytes) {
        clear_exception(env);
        return;
      }
      jsize at = 0;
      for (size_t i = 0; i < event->video.nal_count; i++) {
        const tinyrtc_nal *nal = &event->video.nals[i];
        (*env)->SetByteArrayRegion(env, bytes, at, sizeof start, start);
        (*env)->SetByteArrayRegion(env, bytes, at + (jsize)sizeof start, (jsize)nal->len, (const jbyte *)nal->data);
        at += (jsize)(sizeof start + nal->len);
      }
      a = (jint)event->video.rotation | (event->video.keyframe ? 0x10000 : 0);
      b = (jint)event->video.rtp_timestamp;
      c = (jlong)event->video.play_delay_us;
      break;
    }
    case TINYRTC_EVENT_CHANNEL_OPEN:
      a = (jint)event->channel.id;
      bytes = text_bytes(env, event->channel.label);
      break;
    case TINYRTC_EVENT_CHANNEL_MESSAGE:
      a = (jint)event->message.id;
      b = event->message.binary;
      bytes = new_bytes(env, event->message.data, event->message.len);
      if (!bytes) return;
      break;
    case TINYRTC_EVENT_CHANNEL_BUFFERED_LOW:
    case TINYRTC_EVENT_CHANNEL_CLOSED:
      a = (jint)event->channel.id;
      break;
    default:
      return;
  }
  (*env)->CallStaticVoidMethod(env, native_class, on_connection_event, KEY(user), (jint)event->type, a, b, c, bytes);
  clear_exception(env);
  // A natively attached thread never returns to Java: its local references live until it detaches.
  if (bytes) (*env)->DeleteLocalRef(env, bytes);
}

static void on_target(void *user, uint32_t bitrate_bps, uint32_t max_fps, uint32_t max_height) {
  JNIEnv *env = thread_env();
  if (!env) return;
  (*env)->CallStaticVoidMethod(env, native_class, on_source_target, KEY(user), (jint)bitrate_bps, (jint)max_fps,
                               (jint)max_height);
  clear_exception(env);
}

static void on_keyframe(void *user) {
  JNIEnv *env = thread_env();
  if (!env) return;
  (*env)->CallStaticVoidMethod(env, native_class, on_source_keyframe, KEY(user));
  clear_exception(env);
}

// ---- natives ----------------------------------------------------------------------------------------

static jint n_abi_version(JNIEnv *env, jclass cls) { return (jint)tinyrtc_abi_version(); }

static jlong n_context_create(JNIEnv *env, jclass cls, jint log_level) {
  tinyrtc_context_config config = {on_log, (tinyrtc_log_level)log_level, NULL};
  return (jlong)(intptr_t)tinyrtc_context_create(&config);
}

static void n_context_destroy(JNIEnv *env, jclass cls, jlong context) { tinyrtc_context_destroy(CONTEXT(context)); }

static void n_network_changed(JNIEnv *env, jclass cls, jlong context) {
  tinyrtc_context_network_changed(CONTEXT(context));
}

static void n_audio_push(JNIEnv *env, jclass cls, jlong context, jobject buffer, jint frames) {
  const int16_t *pcm = (*env)->GetDirectBufferAddress(env, buffer);
  if (!pcm || frames <= 0 || (jlong)frames * 2 > (*env)->GetDirectBufferCapacity(env, buffer)) return;
  tinyrtc_audio_push(CONTEXT(context), pcm, (size_t)frames);
}

static void n_audio_pull(JNIEnv *env, jclass cls, jlong context, jobject buffer, jint frames) {
  int16_t *pcm = (*env)->GetDirectBufferAddress(env, buffer);
  if (!pcm || frames <= 0 || (jlong)frames * 2 > (*env)->GetDirectBufferCapacity(env, buffer)) return;
  tinyrtc_audio_pull(CONTEXT(context), pcm, (size_t)frames);
}

static jlong n_source_create(JNIEnv *env, jclass cls, jlong context, jint key) {
  tinyrtc_video_source_callbacks callbacks = {on_target, on_keyframe, (void *)(intptr_t)key};
  return (jlong)(intptr_t)tinyrtc_video_source_create(CONTEXT(context), &callbacks);
}

static void n_source_destroy(JNIEnv *env, jclass cls, jlong source) { tinyrtc_video_source_destroy(SOURCE(source)); }

static void n_source_set_limits(JNIEnv *env, jclass cls, jlong source, jint bitrate, jint fps, jint height) {
  tinyrtc_video_source_set_limits(SOURCE(source), (uint32_t)bitrate, (uint32_t)fps, (uint32_t)height);
}

static void n_source_send(JNIEnv *env, jclass cls, jlong source, jobject buffer, jint offset, jint length,
                          jlong capture_time_us, jboolean keyframe, jint rotation) {
  uint8_t *base = (*env)->GetDirectBufferAddress(env, buffer);
  if (!base || offset < 0 || length <= 0 || (jlong)offset + length > (*env)->GetDirectBufferCapacity(env, buffer)) return;
  tinyrtc_video_frame_in frame = {
      .data = base + offset,
      .len = (size_t)length,
      .framing = TINYRTC_H264_ANNEX_B,
      .capture_time_us = (int64_t)capture_time_us,
      .keyframe = keyframe ? 1 : 0,
      .rotation = (uint16_t)rotation,
  };
  tinyrtc_video_source_send(SOURCE(source), &frame);
}

static jlong n_connection_create(JNIEnv *env, jclass cls, jlong context, jint key, jobjectArray urls,
                                 jobjectArray usernames, jobjectArray credentials, jboolean relay_only) {
  jsize count = urls ? (*env)->GetArrayLength(env, urls) : 0;
  tinyrtc_ice_server *servers = count > 0 ? calloc((size_t)count, sizeof *servers) : NULL;
  if (!servers) count = 0;
  for (jsize i = 0; i < count; i++) {
    servers[i].url = c_text_at(env, urls, i);
    servers[i].username = c_text_at(env, usernames, i);
    servers[i].credential = c_text_at(env, credentials, i);
  }
  tinyrtc_connection_config config = {
      .ice_servers = servers,
      .ice_server_count = (size_t)count,
      .relay_only = relay_only ? 1 : 0,
      .event = on_event,
      .user = (void *)(intptr_t)key,
  };
  tinyrtc_connection *connection = tinyrtc_connection_create(CONTEXT(context), &config);  // copies the servers
  for (jsize i = 0; i < count; i++) {
    free((void *)servers[i].url);
    free((void *)servers[i].username);
    free((void *)servers[i].credential);
  }
  free(servers);
  return (jlong)(intptr_t)connection;
}

static void n_connection_destroy(JNIEnv *env, jclass cls, jlong connection) {
  tinyrtc_connection_destroy(CONNECTION(connection));
}

static jbyteArray n_offer(JNIEnv *env, jclass cls, jlong connection, jboolean ice_restart, jintArray status) {
  tinyrtc_text offer = {NULL, 0};
  jint result = tinyrtc_connection_offer(CONNECTION(connection), ice_restart ? 1 : 0, &offer);
  set_status(env, status, result);
  return take_text(env, offer, result);
}

static jbyteArray n_accept_offer(JNIEnv *env, jclass cls, jlong connection, jbyteArray offer, jintArray status) {
  char *text = c_text(env, offer);
  tinyrtc_text answer = {NULL, 0};
  jint result = text ? tinyrtc_connection_accept_offer(CONNECTION(connection), text, &answer) : TINYRTC_ERR_ARGUMENT;
  free(text);
  set_status(env, status, result);
  return take_text(env, answer, result);
}

static jint n_accept_answer(JNIEnv *env, jclass cls, jlong connection, jbyteArray answer) {
  char *text = c_text(env, answer);
  jint result = text ? tinyrtc_connection_accept_answer(CONNECTION(connection), text) : TINYRTC_ERR_ARGUMENT;
  free(text);
  return result;
}

static jint n_add_candidate(JNIEnv *env, jclass cls, jlong connection, jbyteArray candidate) {
  char *text = c_text(env, candidate);  // NULL = the other side has no more
  jint result = tinyrtc_connection_add_candidate(CONNECTION(connection), text);
  free(text);
  return result;
}

static void n_send_audio(JNIEnv *env, jclass cls, jlong connection, jboolean on) {
  tinyrtc_connection_send_audio(CONNECTION(connection), on ? 1 : 0);
}

static void n_send_video(JNIEnv *env, jclass cls, jlong connection, jlong source) {
  tinyrtc_connection_send_video(CONNECTION(connection), SOURCE(source));
}

static void n_mute_playout(JNIEnv *env, jclass cls, jlong connection, jboolean muted) {
  tinyrtc_connection_mute_playout(CONNECTION(connection), muted ? 1 : 0);
}

static void n_request_keyframe(JNIEnv *env, jclass cls, jlong connection) {
  tinyrtc_connection_request_keyframe(CONNECTION(connection));
}

// The channel's id (>= 0), or a status (< 0).
static jlong n_channel_open(JNIEnv *env, jclass cls, jlong connection, jbyteArray label, jboolean unordered,
                            jint max_retransmits, jint max_packet_life_ms) {
  char *text = c_text(env, label);
  if (!text) return TINYRTC_ERR_ARGUMENT;
  tinyrtc_channel_options options = {unordered ? 1 : 0, max_retransmits, max_packet_life_ms};
  uint32_t id = 0;
  jint status = tinyrtc_channel_open(CONNECTION(connection), text, &options, &id);
  free(text);
  return status == TINYRTC_OK ? (jlong)id : (jlong)status;
}

static jint n_channel_send(JNIEnv *env, jclass cls, jlong connection, jint id, jbyteArray data, jboolean binary) {
  jsize len = data ? (*env)->GetArrayLength(env, data) : 0;
  jbyte *bytes = data ? (*env)->GetByteArrayElements(env, data, NULL) : NULL;
  jint status = tinyrtc_channel_send(CONNECTION(connection), (uint32_t)id, (const uint8_t *)bytes, (size_t)len, binary ? 1 : 0);
  if (bytes) (*env)->ReleaseByteArrayElements(env, data, bytes, JNI_ABORT);
  return status;
}

// Bytes queued (>= 0), or a status (< 0).
static jlong n_channel_buffered(JNIEnv *env, jclass cls, jlong connection, jint id) {
  uint64_t bytes = 0;
  jint status = tinyrtc_channel_buffered(CONNECTION(connection), (uint32_t)id, &bytes);
  return status == TINYRTC_OK ? (jlong)(bytes > INT64_MAX ? INT64_MAX : bytes) : (jlong)status;
}

static jint n_channel_set_buffered_low(JNIEnv *env, jclass cls, jlong connection, jint id, jlong threshold) {
  return tinyrtc_channel_set_buffered_low(CONNECTION(connection), (uint32_t)id, threshold < 0 ? 0 : (uint64_t)threshold);
}

static jint n_channel_close(JNIEnv *env, jclass cls, jlong connection, jint id) {
  return tinyrtc_channel_close(CONNECTION(connection), (uint32_t)id);
}

// The stats struct, field for field in the header's order, as doubles (every value fits exactly).
static jint n_stats(JNIEnv *env, jclass cls, jlong connection, jdoubleArray out) {
  enum { COUNT = 16 };
  if (!out || (*env)->GetArrayLength(env, out) < COUNT) return TINYRTC_ERR_ARGUMENT;
  tinyrtc_stats s;
  memset(&s, 0, sizeof s);
  jint status = tinyrtc_connection_stats(CONNECTION(connection), &s);
  if (status != TINYRTC_OK) return status;
  jdouble v[COUNT] = {
      s.state,          s.relayed,        s.rtt_ms,     s.send_estimate_bps,
      s.audio_send_bps, s.audio_recv_bps, s.video_send_bps,
      s.video_recv_bps, s.audio_loss,     s.video_loss, s.audio_jitter_buffer_ms,
      s.video_recv_fps, s.video_nacks_sent, s.video_keyframes_requested,
      (jdouble)s.bytes_sent, (jdouble)s.bytes_received,
  };
  (*env)->SetDoubleArrayRegion(env, out, 0, COUNT, v);
  return status;
}

static const JNINativeMethod methods[] = {
    {"abiVersion", "()I", (void *)n_abi_version},
    {"contextCreate", "(I)J", (void *)n_context_create},
    {"contextDestroy", "(J)V", (void *)n_context_destroy},
    {"networkChanged", "(J)V", (void *)n_network_changed},
    {"audioPush", "(JLjava/nio/ByteBuffer;I)V", (void *)n_audio_push},
    {"audioPull", "(JLjava/nio/ByteBuffer;I)V", (void *)n_audio_pull},
    {"sourceCreate", "(JI)J", (void *)n_source_create},
    {"sourceDestroy", "(J)V", (void *)n_source_destroy},
    {"sourceSetLimits", "(JIII)V", (void *)n_source_set_limits},
    {"sourceSend", "(JLjava/nio/ByteBuffer;IIJZI)V", (void *)n_source_send},
    {"connectionCreate", "(JI[[B[[B[[BZ)J", (void *)n_connection_create},
    {"connectionDestroy", "(J)V", (void *)n_connection_destroy},
    {"offer", "(JZ[I)[B", (void *)n_offer},
    {"acceptOffer", "(J[B[I)[B", (void *)n_accept_offer},
    {"acceptAnswer", "(J[B)I", (void *)n_accept_answer},
    {"addCandidate", "(J[B)I", (void *)n_add_candidate},
    {"sendAudio", "(JZ)V", (void *)n_send_audio},
    {"sendVideo", "(JJ)V", (void *)n_send_video},
    {"mutePlayout", "(JZ)V", (void *)n_mute_playout},
    {"requestKeyframe", "(J)V", (void *)n_request_keyframe},
    {"channelOpen", "(J[BZII)J", (void *)n_channel_open},
    {"channelSend", "(JI[BZ)I", (void *)n_channel_send},
    {"channelBuffered", "(JI)J", (void *)n_channel_buffered},
    {"channelSetBufferedLow", "(JIJ)I", (void *)n_channel_set_buffered_low},
    {"channelClose", "(JI)I", (void *)n_channel_close},
    {"stats", "(J[D)I", (void *)n_stats},
};

JNIEXPORT jint JNI_OnLoad(JavaVM *jvm, void *reserved) {
  (void)reserved;
  vm = jvm;
  JNIEnv *env = NULL;
  if ((*vm)->GetEnv(vm, (void **)&env, JNI_VERSION_1_6) != JNI_OK) return JNI_ERR;
  if (tinyrtc_abi_version() != TINYRTC_ABI_VERSION) {
    __android_log_print(ANDROID_LOG_ERROR, TAG, "core ABI %u, shim built for %d", tinyrtc_abi_version(), TINYRTC_ABI_VERSION);
    return JNI_ERR;
  }
  // Found here, on the loading thread: a natively attached thread's FindClass sees only the system
  // class loader, not the app's.
  jclass cls = (*env)->FindClass(env, "dev/tinyrtc/Native");
  if (!cls) return JNI_ERR;
  native_class = (jclass)(*env)->NewGlobalRef(env, cls);
  (*env)->DeleteLocalRef(env, cls);
  on_connection_event = (*env)->GetStaticMethodID(env, native_class, "onConnectionEvent", "(IIIIJ[B)V");
  on_source_target = (*env)->GetStaticMethodID(env, native_class, "onSourceTarget", "(IIII)V");
  on_source_keyframe = (*env)->GetStaticMethodID(env, native_class, "onSourceKeyframe", "(I)V");
  if (!on_connection_event || !on_source_target || !on_source_keyframe) return JNI_ERR;
  if ((*env)->RegisterNatives(env, native_class, methods, sizeof methods / sizeof methods[0]) != JNI_OK) return JNI_ERR;
  if (pthread_key_create(&detach_key, detach_thread) != 0) return JNI_ERR;
  return JNI_VERSION_1_6;
}
