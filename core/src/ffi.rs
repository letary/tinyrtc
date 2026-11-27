//! The C face: `include/tinyrtc.h`, function for function. Nothing is decided here — a handle
//! is a box around the Rust object of the same name, an event is the Rust event laid out as the
//! header's struct for the length of one callback.
//!
//! The header is the contract; this file must match it by hand (no generator). The layout test at
//! the bottom pins the sizes the header implies, and tests/ffi.rs drives a whole call through
//! these functions.

#![allow(non_camel_case_types, clippy::missing_safety_doc)]

use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr;

use crate::connection::{ChannelOptions, Config, Error, IceServer, Media, State};
use crate::context::{Context, Event, FrameIn, Peer, SourceCallbacks, Target, VideoSource};
use crate::h264::{self, Framing};

pub const TINYRTC_ABI_VERSION: u32 = 1;

pub const TINYRTC_OK: i32 = 0;
pub const TINYRTC_ERR_HANDLE: i32 = -1;
pub const TINYRTC_ERR_ARGUMENT: i32 = -2;
pub const TINYRTC_ERR_STATE: i32 = -3;
pub const TINYRTC_ERR_SDP: i32 = -4;
pub const TINYRTC_ERR_NOT_FOUND: i32 = -5;
pub const TINYRTC_ERR_CLOSED: i32 = -6;

pub const TINYRTC_EVENT_STATE: i32 = 1;
pub const TINYRTC_EVENT_LOCAL_CANDIDATE: i32 = 2;
pub const TINYRTC_EVENT_ICE_RESTART_NEEDED: i32 = 3;
pub const TINYRTC_EVENT_REMOTE_MEDIA: i32 = 4;
pub const TINYRTC_EVENT_VIDEO_FRAME: i32 = 5;
pub const TINYRTC_EVENT_CHANNEL_OPEN: i32 = 6;
pub const TINYRTC_EVENT_CHANNEL_MESSAGE: i32 = 7;
pub const TINYRTC_EVENT_CHANNEL_BUFFERED_LOW: i32 = 8;
pub const TINYRTC_EVENT_CHANNEL_CLOSED: i32 = 9;

fn status(error: Error) -> i32 {
    match error {
        Error::Argument => TINYRTC_ERR_ARGUMENT,
        Error::State => TINYRTC_ERR_STATE,
        Error::Sdp => TINYRTC_ERR_SDP,
        Error::NotFound => TINYRTC_ERR_NOT_FOUND,
        Error::Closed => TINYRTC_ERR_CLOSED,
    }
}

fn state(state: State) -> i32 {
    match state {
        State::New => 0,
        State::Connecting => 1,
        State::Connected => 2,
        State::Disconnected => 3,
        State::Failed => 4,
        State::Closed => 5,
    }
}

/// The caller's pointer, carried to the callbacks on our threads: the header says they run there.
#[derive(Clone, Copy)]
struct User(*mut c_void);
unsafe impl Send for User {}

// ---- text ------------------------------------------------------------------------------------------

#[repr(C)]
pub struct tinyrtc_text {
    pub data: *mut c_char,
    pub len: usize,
}

fn text(s: String) -> tinyrtc_text {
    // An SDP holds no NUL; were one there, the text ends at it.
    let s = CString::new(s).unwrap_or_else(|e| {
        let at = e.nul_position();
        let mut bytes = e.into_vec();
        bytes.truncate(at);
        CString::new(bytes).unwrap_or_default()
    });
    let len = s.as_bytes().len();
    tinyrtc_text { data: s.into_raw(), len }
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_text_free(text: tinyrtc_text) {
    if !text.data.is_null() {
        drop(CString::from_raw(text.data));
    }
}

unsafe fn str_of<'a>(s: *const c_char) -> Option<&'a str> {
    if s.is_null() {
        return None;
    }
    CStr::from_ptr(s).to_str().ok()
}

#[no_mangle]
pub extern "C" fn tinyrtc_abi_version() -> u32 {
    TINYRTC_ABI_VERSION
}

// ---- the context -----------------------------------------------------------------------------------

#[repr(C)]
pub struct tinyrtc_context_config {
    pub log: Option<unsafe extern "C" fn(user: *mut c_void, level: i32, line: *const c_char)>,
    pub log_level: i32,
    pub user: *mut c_void,
}

#[derive(Clone, Copy)]
struct Log {
    call: Option<unsafe extern "C" fn(*mut c_void, i32, *const c_char)>,
    level: i32,
    user: User,
}

impl Log {
    fn warn(&self, line: &str) {
        const WARN: i32 = 2;
        if let (Some(call), true) = (self.call, self.level >= WARN) {
            if let Ok(line) = CString::new(line) {
                unsafe { call(self.user.0, WARN, line.as_ptr()) };
            }
        }
    }
}

pub struct tinyrtc_context {
    context: Context,
    log: Log,
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_context_create(config: *const tinyrtc_context_config) -> *mut tinyrtc_context {
    let log = match config.as_ref() {
        Some(config) => Log { call: config.log, level: config.log_level, user: User(config.user) },
        None => Log { call: None, level: 0, user: User(ptr::null_mut()) },
    };
    Box::into_raw(Box::new(tinyrtc_context { context: Context::new(), log }))
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_context_destroy(context: *mut tinyrtc_context) {
    if !context.is_null() {
        drop(Box::from_raw(context));
    }
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_context_network_changed(context: *mut tinyrtc_context) {
    if let Some(context) = context.as_ref() {
        context.context.network_changed();
    }
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_audio_push(context: *mut tinyrtc_context, pcm: *const i16, frames: usize) {
    if let (Some(context), false) = (context.as_ref(), pcm.is_null()) {
        context.context.engine().push(std::slice::from_raw_parts(pcm, frames));
    }
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_audio_pull(context: *mut tinyrtc_context, pcm: *mut i16, frames: usize) {
    if pcm.is_null() {
        return;
    }
    let out = std::slice::from_raw_parts_mut(pcm, frames);
    match context.as_ref() {
        Some(context) => context.context.engine().pull(out),
        None => out.fill(0),
    }
}

// ---- video out -------------------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
pub struct tinyrtc_video_source_callbacks {
    pub target: Option<unsafe extern "C" fn(user: *mut c_void, bitrate_bps: u32, max_fps: u32, max_height: u32)>,
    pub keyframe: Option<unsafe extern "C" fn(user: *mut c_void)>,
    pub user: *mut c_void,
}

struct Callbacks {
    target: Option<unsafe extern "C" fn(*mut c_void, u32, u32, u32)>,
    keyframe: Option<unsafe extern "C" fn(*mut c_void)>,
    user: User,
}

impl SourceCallbacks for Callbacks {
    fn target(&mut self, target: Target) {
        if let Some(call) = self.target {
            unsafe { call(self.user.0, target.bitrate_bps, target.max_fps, target.max_height) };
        }
    }
    fn keyframe(&mut self) {
        if let Some(call) = self.keyframe {
            unsafe { call(self.user.0) };
        }
    }
}

pub struct tinyrtc_video_source {
    source: VideoSource,
    log: Log,
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_video_source_create(
    context: *mut tinyrtc_context,
    callbacks: *const tinyrtc_video_source_callbacks,
) -> *mut tinyrtc_video_source {
    let Some(context) = context.as_ref() else { return ptr::null_mut() };
    let callbacks = match callbacks.as_ref() {
        Some(c) => Callbacks { target: c.target, keyframe: c.keyframe, user: User(c.user) },
        None => Callbacks { target: None, keyframe: None, user: User(ptr::null_mut()) },
    };
    Box::into_raw(Box::new(tinyrtc_video_source { source: VideoSource::new(Box::new(callbacks)), log: context.log }))
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_video_source_destroy(source: *mut tinyrtc_video_source) {
    if !source.is_null() {
        let source = Box::from_raw(source);
        source.source.close();
    }
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_video_source_set_limits(
    source: *mut tinyrtc_video_source,
    max_bitrate_bps: u32,
    max_fps: u32,
    max_height: u32,
) {
    if let Some(source) = source.as_ref() {
        source.source.set_limits(max_bitrate_bps, max_fps, max_height);
    }
}

#[repr(C)]
pub struct tinyrtc_video_frame_in {
    pub data: *const u8,
    pub len: usize,
    pub framing: i32,
    pub capture_time_us: i64,
    pub keyframe: u8,
    pub rotation: u16,
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_video_source_send(source: *mut tinyrtc_video_source, frame: *const tinyrtc_video_frame_in) {
    let (Some(source), Some(frame)) = (source.as_ref(), frame.as_ref()) else { return };
    if frame.data.is_null() {
        return;
    }
    let frame = FrameIn {
        data: std::slice::from_raw_parts(frame.data, frame.len),
        framing: if frame.framing == 1 { Framing::Avcc } else { Framing::AnnexB },
        capture_time_us: frame.capture_time_us,
        keyframe: frame.keyframe != 0,
        rotation: frame.rotation,
    };
    if let Err(error) = source.source.send(&frame) {
        source.log.warn(&format!("video frame not sent: {error:?}"));
    }
}

// ---- a connection ----------------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
pub struct tinyrtc_nal {
    pub data: *const u8,
    pub len: usize,
    pub kind: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct tinyrtc_event_candidate {
    pub candidate: *const c_char,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct tinyrtc_event_remote_media {
    pub media: i32,
    pub active: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct tinyrtc_event_video {
    pub nals: *const tinyrtc_nal,
    pub nal_count: usize,
    pub keyframe: u8,
    pub rotation: u16,
    pub play_delay_us: i64,
    pub rtp_timestamp: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct tinyrtc_event_channel {
    pub id: u32,
    pub label: *const c_char,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct tinyrtc_event_message {
    pub id: u32,
    pub data: *const u8,
    pub len: usize,
    pub binary: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union tinyrtc_event_body {
    pub state: i32,
    pub candidate: tinyrtc_event_candidate,
    pub remote_media: tinyrtc_event_remote_media,
    pub video: tinyrtc_event_video,
    pub channel: tinyrtc_event_channel,
    pub message: tinyrtc_event_message,
}

#[repr(C)]
pub struct tinyrtc_event {
    pub kind: i32,
    pub body: tinyrtc_event_body,
}

#[repr(C)]
pub struct tinyrtc_ice_server {
    pub url: *const c_char,
    pub username: *const c_char,
    pub credential: *const c_char,
}

#[repr(C)]
pub struct tinyrtc_connection_config {
    pub ice_servers: *const tinyrtc_ice_server,
    pub ice_server_count: usize,
    pub relay_only: u8,
    pub event: Option<unsafe extern "C" fn(user: *mut c_void, event: *const tinyrtc_event)>,
    pub user: *mut c_void,
}

/// No lock around the peer: its methods take `&self` and are safe from any thread. A lock here
/// would be held by a SYNC call while it waits for the connection's thread — and a callback on
/// that thread calling any function of the connection would then wait for the lock forever.
pub struct tinyrtc_connection {
    peer: Peer,
}

/// One Rust event as the header's struct, alive for the call.
unsafe fn deliver(call: unsafe extern "C" fn(*mut c_void, *const tinyrtc_event), user: User, event: Event, delay_us: i64) {
    let send = |kind: i32, body: tinyrtc_event_body| call(user.0, &tinyrtc_event { kind, body });
    match event {
        Event::State(s) => send(TINYRTC_EVENT_STATE, tinyrtc_event_body { state: state(s) }),
        Event::LocalCandidate(line) => {
            let Ok(line) = CString::new(line) else { return };
            send(TINYRTC_EVENT_LOCAL_CANDIDATE, tinyrtc_event_body { candidate: tinyrtc_event_candidate { candidate: line.as_ptr() } })
        }
        Event::IceRestartNeeded => send(TINYRTC_EVENT_ICE_RESTART_NEEDED, tinyrtc_event_body { state: 0 }),
        Event::RemoteMedia { media, active } => {
            let media = if media == Media::Audio { 0 } else { 1 };
            let remote_media = tinyrtc_event_remote_media { media, active: active as u8 };
            send(TINYRTC_EVENT_REMOTE_MEDIA, tinyrtc_event_body { remote_media })
        }
        Event::VideoFrame(frame) => {
            // str0m hands a frame out as Annex B; the platform frames the units as its decoder wants.
            let nals: Vec<tinyrtc_nal> = h264::nals(&frame.data, Framing::AnnexB)
                .map(|nal| tinyrtc_nal { data: nal.data.as_ptr(), len: nal.data.len(), kind: nal.kind() })
                .collect();
            let video = tinyrtc_event_video {
                nals: nals.as_ptr(),
                nal_count: nals.len(),
                keyframe: frame.keyframe as u8,
                rotation: frame.rotation,
                play_delay_us: delay_us,
                rtp_timestamp: frame.rtp_timestamp,
            };
            send(TINYRTC_EVENT_VIDEO_FRAME, tinyrtc_event_body { video })
        }
        Event::ChannelOpen { id, label } => {
            let label = CString::new(label).unwrap_or_default();
            let channel = tinyrtc_event_channel { id, label: label.as_ptr() };
            send(TINYRTC_EVENT_CHANNEL_OPEN, tinyrtc_event_body { channel })
        }
        Event::ChannelMessage { id, data, binary } => {
            let message = tinyrtc_event_message { id, data: data.as_ptr(), len: data.len(), binary: binary as u8 };
            send(TINYRTC_EVENT_CHANNEL_MESSAGE, tinyrtc_event_body { message })
        }
        Event::ChannelBufferedLow { id } => {
            let channel = tinyrtc_event_channel { id, label: ptr::null() };
            send(TINYRTC_EVENT_CHANNEL_BUFFERED_LOW, tinyrtc_event_body { channel })
        }
        Event::ChannelClosed { id } => {
            let channel = tinyrtc_event_channel { id, label: ptr::null() };
            send(TINYRTC_EVENT_CHANNEL_CLOSED, tinyrtc_event_body { channel })
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_connection_create(
    context: *mut tinyrtc_context,
    config: *const tinyrtc_connection_config,
) -> *mut tinyrtc_connection {
    connection_create_bound(context, config, None)
}

/// `tinyrtc_connection_create` on a chosen local address. Not in the header: the tests use it to
/// stay on the loopback interface.
#[doc(hidden)]
pub unsafe fn connection_create_bound(
    context: *mut tinyrtc_context,
    config: *const tinyrtc_connection_config,
    bind: Option<std::net::IpAddr>,
) -> *mut tinyrtc_connection {
    let (Some(context), Some(config)) = (context.as_ref(), config.as_ref()) else { return ptr::null_mut() };
    let mut ice_servers = Vec::new();
    if !config.ice_servers.is_null() {
        for server in std::slice::from_raw_parts(config.ice_servers, config.ice_server_count) {
            let Some(url) = str_of(server.url) else { continue };
            ice_servers.push(IceServer {
                url: url.to_owned(),
                username: str_of(server.username).map(str::to_owned),
                credential: str_of(server.credential).map(str::to_owned),
            });
        }
    }
    let rust = Config { ice_servers, relay_only: config.relay_only != 0, bind };
    let (call, user) = (config.event, User(config.user));
    // The picture's delay is the sound's: read from the engine at each frame.
    let engine = context.context.clone();
    let delay: std::sync::Arc<std::sync::Mutex<Option<u64>>> = Default::default();
    let stream = delay.clone();
    let sink = Box::new(move |event: Event| {
        let Some(call) = call else { return };
        let user = user;
        let delay_us = match (&event, *stream.lock().unwrap()) {
            (Event::VideoFrame(_), Some(id)) => {
                engine.engine().stream_stats(id).map_or(0, |stats| stats.level_ms as i64 * 1000)
            }
            _ => 0,
        };
        deliver(call, user, event, delay_us);
    });
    match context.context.connect(rust, sink) {
        Ok(peer) => {
            *delay.lock().unwrap() = Some(peer.stream_id());
            Box::into_raw(Box::new(tinyrtc_connection { peer }))
        }
        Err(_) => ptr::null_mut(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_connection_destroy(connection: *mut tinyrtc_connection) {
    if !connection.is_null() {
        drop(Box::from_raw(connection));
    }
}

unsafe fn with<T>(connection: *mut tinyrtc_connection, f: impl FnOnce(&Peer) -> Result<T, Error>) -> Result<T, i32> {
    let connection = connection.as_ref().ok_or(TINYRTC_ERR_HANDLE)?;
    f(&connection.peer).map_err(status)
}

fn code(result: Result<(), i32>) -> i32 {
    result.err().unwrap_or(TINYRTC_OK)
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_connection_offer(connection: *mut tinyrtc_connection, ice_restart: u8, offer: *mut tinyrtc_text) -> i32 {
    if offer.is_null() {
        return TINYRTC_ERR_ARGUMENT;
    }
    code(with(connection, |peer| peer.offer(ice_restart != 0)).map(|sdp| *offer = text(sdp)))
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_connection_accept_offer(
    connection: *mut tinyrtc_connection,
    offer: *const c_char,
    answer: *mut tinyrtc_text,
) -> i32 {
    let (Some(offer), false) = (str_of(offer), answer.is_null()) else { return TINYRTC_ERR_ARGUMENT };
    code(with(connection, |peer| peer.accept_offer(offer)).map(|sdp| *answer = text(sdp)))
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_connection_accept_answer(connection: *mut tinyrtc_connection, answer: *const c_char) -> i32 {
    let Some(answer) = str_of(answer) else { return TINYRTC_ERR_ARGUMENT };
    code(with(connection, |peer| peer.accept_answer(answer)))
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_connection_add_candidate(connection: *mut tinyrtc_connection, candidate: *const c_char) -> i32 {
    // NULL = the other side has no more: nothing to do, the checks end by themselves.
    let Some(candidate) = str_of(candidate) else { return TINYRTC_OK };
    code(with(connection, |peer| {
        peer.add_candidate(candidate);
        Ok(())
    }))
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_connection_send_audio(connection: *mut tinyrtc_connection, on: u8) {
    let _ = with(connection, |peer| {
        peer.send_audio(on != 0);
        Ok(())
    });
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_connection_send_video(connection: *mut tinyrtc_connection, source: *mut tinyrtc_video_source) {
    let source = source.as_ref().map(|source| &source.source);
    let _ = with(connection, |peer| {
        peer.send_video(source);
        Ok(())
    });
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_connection_mute_playout(connection: *mut tinyrtc_connection, muted: u8) {
    let _ = with(connection, |peer| {
        peer.mute_playout(muted != 0);
        Ok(())
    });
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_connection_request_keyframe(connection: *mut tinyrtc_connection) {
    let _ = with(connection, |peer| {
        peer.request_keyframe();
        Ok(())
    });
}

#[repr(C)]
pub struct tinyrtc_channel_options {
    pub unordered: u8,
    pub max_retransmits: i32,
    pub max_packet_life_ms: i32,
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_channel_open(
    connection: *mut tinyrtc_connection,
    label: *const c_char,
    options: *const tinyrtc_channel_options,
    id: *mut u32,
) -> i32 {
    let (Some(label), false) = (str_of(label), id.is_null()) else { return TINYRTC_ERR_ARGUMENT };
    let limit = |v: i32| (v >= 0).then_some(v.min(u16::MAX as i32) as u16);
    let options = match options.as_ref() {
        None => ChannelOptions::default(),
        Some(o) => ChannelOptions {
            unordered: o.unordered != 0,
            max_retransmits: limit(o.max_retransmits),
            max_packet_life_ms: limit(o.max_packet_life_ms),
        },
    };
    code(with(connection, |peer| peer.channel_open(label, options)).map(|opened| *id = opened))
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_channel_send(
    connection: *mut tinyrtc_connection,
    id: u32,
    data: *const u8,
    len: usize,
    binary: u8,
) -> i32 {
    if data.is_null() && len > 0 {
        return TINYRTC_ERR_ARGUMENT;
    }
    let data = if len == 0 { Vec::new() } else { std::slice::from_raw_parts(data, len).to_vec() };
    code(with(connection, |peer| peer.channel_send(id, data, binary != 0)))
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_channel_buffered(connection: *mut tinyrtc_connection, id: u32, bytes: *mut u64) -> i32 {
    if bytes.is_null() {
        return TINYRTC_ERR_ARGUMENT;
    }
    code(with(connection, |peer| peer.channel_buffered(id)).map(|buffered| *bytes = buffered))
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_channel_set_buffered_low(connection: *mut tinyrtc_connection, id: u32, threshold: u64) -> i32 {
    code(with(connection, |peer| {
        peer.channel_set_buffered_low(id, threshold);
        Ok(())
    }))
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_channel_close(connection: *mut tinyrtc_connection, id: u32) -> i32 {
    code(with(connection, |peer| {
        peer.channel_close(id);
        Ok(())
    }))
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct tinyrtc_stats {
    pub state: i32,
    pub relayed: u8,
    pub rtt_ms: u32,
    pub send_estimate_bps: u32,
    pub audio_send_bps: u32,
    pub audio_recv_bps: u32,
    pub video_send_bps: u32,
    pub video_recv_bps: u32,
    pub audio_loss: f32,
    pub video_loss: f32,
    pub audio_jitter_buffer_ms: u32,
    pub video_recv_fps: u32,
    pub video_nacks_sent: u32,
    pub video_keyframes_requested: u32,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

#[no_mangle]
pub unsafe extern "C" fn tinyrtc_connection_stats(connection: *mut tinyrtc_connection, stats: *mut tinyrtc_stats) -> i32 {
    if stats.is_null() {
        return TINYRTC_ERR_ARGUMENT;
    }
    code(with(connection, |peer| peer.stats()).map(|s| {
        let c = s.connection;
        *stats = tinyrtc_stats {
            state: state(c.state),
            relayed: c.relayed as u8,
            rtt_ms: c.rtt_ms,
            send_estimate_bps: c.send_estimate_bps,
            audio_send_bps: c.audio_send_bps,
            audio_recv_bps: c.audio_recv_bps,
            video_send_bps: c.video_send_bps,
            video_recv_bps: c.video_recv_bps,
            audio_loss: c.audio_loss,
            video_loss: c.video_loss,
            audio_jitter_buffer_ms: s.audio_jitter_buffer_ms,
            video_recv_fps: c.video_recv_fps,
            video_nacks_sent: c.video_nacks_sent,
            video_keyframes_requested: c.video_keyframes_requested,
            bytes_sent: c.bytes_sent,
            bytes_received: c.bytes_received,
        };
    }))
}
