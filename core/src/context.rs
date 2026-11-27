//! What is shared by the peers of one app: the sound (audio::engine) and the local video sources.
//! A `Peer` is a connection wired into both — its incoming Opus goes to a stream of the engine,
//! the microphone and a video source are fanned out to it when it takes them, and what the network
//! says about the way to that peer (the estimate, a lost picture) goes back to the source.
//!
//! A video source is encoded ONCE, by the platform, for every peer that takes it. So the encoder
//! is steered by the weakest of them: the target is the smallest estimate, mapped onto a ladder of
//! sizes and rates, and a keyframe one peer needs is a keyframe for all.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::audio::engine::{Engine, StreamInput};
use crate::audio::jitter::Packet;
use crate::connection::{self, AudioOut, ChannelOptions, Config, Connection, Error, MediaSender, Media, State, VideoOut};
use crate::h264::{self, Framing};

/// The ceilings of a source until the app says otherwise.
pub const DEFAULT_MAX_BITRATE: u32 = 1_500_000;
pub const DEFAULT_MAX_FPS: u32 = 30;
pub const DEFAULT_MAX_HEIGHT: u32 = 720;

/// The share of the estimate given to video: the rest is the sound, the headers, and a margin for
/// an encoder that overshoots (hardware ones do).
const VIDEO_SHARE: f64 = 0.80;
/// Kept for the sound out of any estimate, bits per second.
const AUDIO_RESERVE: u64 = 40_000;
/// A target is announced again when the bitrate moved by this share, or the size / rate changed.
const TARGET_STEP: f64 = 0.10;
const KEYFRAME_EVERY: Duration = Duration::from_secs(1);

/// The ladder: with at least `.0` bits per second, a picture `.1` high at `.2` frames per second.
const LADDER: [(u32, u32, u32); 5] =
    [(1_000_000, 720, 30), (500_000, 480, 30), (250_000, 360, 30), (120_000, 240, 20), (0, 180, 15)];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub bitrate_bps: u32,
    pub max_fps: u32,
    pub max_height: u32,
}

/// What the platform's encoder is told.
pub trait SourceCallbacks: Send {
    fn target(&mut self, target: Target);
    fn keyframe(&mut self);
}

#[derive(Clone, Copy)]
struct Limits {
    bitrate: u32,
    fps: u32,
    height: u32,
}

struct SourceState {
    callbacks: Box<dyn SourceCallbacks>,
    limits: Limits,
    takers: Vec<(u64, MediaSender)>,
    /// The last estimate of each taker, by its peer id.
    estimates: HashMap<u64, u64>,
    announced: Option<Target>,
    last_keyframe: Option<Instant>,
    /// The caller's clock at its first frame, and ours at that moment.
    clock: Option<(i64, Instant)>,
    scratch: Vec<u8>,
    /// The owner let go of it: no callback from now on, whoever still holds a clone.
    closed: bool,
}

impl SourceState {
    fn target(&self) -> Target {
        let limits = self.limits;
        let ceiling = if limits.bitrate == 0 { u32::MAX } else { limits.bitrate };
        // Before any taker reported, the profile's starting estimate.
        let estimate = self
            .takers
            .iter()
            .filter_map(|(id, _)| self.estimates.get(id).copied())
            .min()
            .unwrap_or(crate::profile::START_BITRATE.as_u64());
        let video = (estimate.saturating_sub(AUDIO_RESERVE) as f64 * VIDEO_SHARE) as u64;
        let bitrate = (video.min(ceiling as u64) as u32).max(50_000);
        let (_, height, fps) = LADDER.iter().copied().find(|(floor, _, _)| bitrate >= *floor).unwrap_or(LADDER[4]);
        Target {
            bitrate_bps: bitrate,
            max_fps: if limits.fps == 0 { fps } else { fps.min(limits.fps) },
            max_height: if limits.height == 0 { height } else { height.min(limits.height) },
        }
    }

    fn announce(&mut self) {
        // With nobody taking the stream there is nothing to steer by: the first target comes with
        // the first taker.
        if self.takers.is_empty() || self.closed {
            return;
        }
        let target = self.target();
        let moved = match self.announced {
            None => true,
            Some(before) => {
                let step = (target.bitrate_bps as f64 - before.bitrate_bps as f64).abs() / before.bitrate_bps as f64;
                step >= TARGET_STEP || target.max_fps != before.max_fps || target.max_height != before.max_height
            }
        };
        if moved {
            self.announced = Some(target);
            self.callbacks.target(target);
        }
    }

    fn keyframe(&mut self) {
        if self.closed {
            return;
        }
        let now = Instant::now();
        if self.last_keyframe.is_some_and(|at| now < at + KEYFRAME_EVERY) {
            return;
        }
        self.last_keyframe = Some(now);
        self.callbacks.keyframe();
    }

    /// What the takers should probe up to: the ceiling, plus the sound.
    fn desired(&self) -> u64 {
        let ceiling = if self.limits.bitrate == 0 { DEFAULT_MAX_BITRATE * 2 } else { self.limits.bitrate };
        (ceiling as f64 / VIDEO_SHARE) as u64 + AUDIO_RESERVE
    }
}

/// One encoded H.264 stream going out: the camera.
#[derive(Clone)]
pub struct VideoSource {
    state: Arc<Mutex<SourceState>>,
}

/// One access unit from the platform's encoder.
pub struct FrameIn<'a> {
    pub data: &'a [u8],
    pub framing: Framing,
    /// The caller's monotonic clock, microseconds: only differences are used.
    pub capture_time_us: i64,
    pub keyframe: bool,
    pub rotation: u16,
}

/// Why a frame was not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// The bytes are not what the framing says.
    Malformed,
    /// Marked a keyframe, but without its SPS and PPS: a peer that just came could not decode it.
    KeyframeWithoutParameterSets,
}

impl VideoSource {
    pub fn new(callbacks: Box<dyn SourceCallbacks>) -> VideoSource {
        VideoSource {
            state: Arc::new(Mutex::new(SourceState {
                callbacks,
                limits: Limits { bitrate: DEFAULT_MAX_BITRATE, fps: DEFAULT_MAX_FPS, height: DEFAULT_MAX_HEIGHT },
                takers: Vec::new(),
                estimates: HashMap::new(),
                announced: None,
                last_keyframe: None,
                clock: None,
                scratch: Vec::new(),
                closed: false,
            })),
        }
    }

    /// The end of the source: every taker is let go, and — the callbacks run under the same lock —
    /// once this returns no callback is running or will run, even while a peer still holds the
    /// source it was sending.
    pub fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        state.takers.clear();
        state.estimates.clear();
    }

    /// 0 = no ceiling of that kind.
    pub fn set_limits(&self, max_bitrate_bps: u32, max_fps: u32, max_height: u32) {
        let mut state = self.state.lock().unwrap();
        state.limits = Limits { bitrate: max_bitrate_bps, fps: max_fps, height: max_height };
        let desired = state.desired();
        for (_, taker) in &state.takers {
            taker.set_desired_bitrate(desired);
        }
        state.announce();
    }

    /// Copies the frame and hands it to every taker.
    pub fn send(&self, frame: &FrameIn) -> Result<(), FrameError> {
        let mut guard = self.state.lock().unwrap();
        let state = &mut *guard;
        if state.takers.is_empty() || state.closed {
            return Ok(());
        }
        if frame.keyframe && !h264::has_parameter_sets(frame.data, frame.framing) {
            return Err(FrameError::KeyframeWithoutParameterSets);
        }
        if !h264::to_packetizer(frame.data, frame.framing, &mut state.scratch) {
            return Err(FrameError::Malformed);
        }
        let (first, anchor) = *state.clock.get_or_insert((frame.capture_time_us, Instant::now()));
        let captured = anchor + Duration::from_micros(frame.capture_time_us.saturating_sub(first).max(0) as u64);
        let out = VideoOut { data: Arc::from(state.scratch.as_slice()), captured, rotation: frame.rotation };
        for (_, taker) in &state.takers {
            taker.send_video(out.clone());
        }
        Ok(())
    }

    fn attach(&self, peer: u64, taker: MediaSender) {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return;
        }
        taker.set_desired_bitrate(state.desired());
        state.takers.retain(|(id, _)| *id != peer);
        state.takers.push((peer, taker));
        // A new receiver can start only at a keyframe.
        state.last_keyframe = None;
        state.keyframe();
        state.announce();
    }

    fn detach(&self, peer: u64) {
        let mut state = self.state.lock().unwrap();
        state.takers.retain(|(id, _)| *id != peer);
        state.estimates.remove(&peer);
        state.announce();
    }
}

fn estimate(source: &Weak<Mutex<SourceState>>, peer: u64, bps: u64) {
    if let Some(state) = source.upgrade() {
        let mut state = state.lock().unwrap();
        state.estimates.insert(peer, bps);
        state.announce();
    }
}

/// A keyframe now, whatever the rate limit says: a receiver has nothing to start from.
fn fresh_keyframe(source: &Weak<Mutex<SourceState>>) {
    if let Some(state) = source.upgrade() {
        let mut state = state.lock().unwrap();
        state.last_keyframe = None;
        state.keyframe();
    }
}

fn keyframe(source: &Weak<Mutex<SourceState>>) {
    if let Some(state) = source.upgrade() {
        state.lock().unwrap().keyframe();
    }
}

/// What a peer tells the app (the connection's events, minus what the context consumed).
#[derive(Debug, Clone)]
pub enum Event {
    State(State),
    LocalCandidate(String),
    IceRestartNeeded,
    RemoteMedia { media: Media, active: bool },
    VideoFrame(connection::VideoFrame),
    ChannelOpen { id: u32, label: String },
    ChannelMessage { id: u32, data: Vec<u8>, binary: bool },
    ChannelBufferedLow { id: u32 },
    ChannelClosed { id: u32 },
}

#[derive(Clone, Default)]
pub struct Context {
    engine: Engine,
    peers: Arc<Mutex<Vec<(u64, MediaSender)>>>,
    next_peer: Arc<Mutex<u64>>,
}

impl Context {
    pub fn new() -> Context {
        Context::default()
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// The OS reported a change of network: every peer looks at its route again.
    pub fn network_changed(&self) {
        for (_, peer) in self.peers.lock().unwrap().iter() {
            peer.network_changed();
        }
    }

    /// `sink` gets the peer's events, on the peer's own thread.
    pub fn connect(&self, config: Config, mut sink: Box<dyn FnMut(Event) + Send>) -> std::io::Result<Peer> {
        let id = {
            let mut next = self.next_peer.lock().unwrap();
            *next += 1;
            *next
        };
        let stream = self.engine.add_stream();
        let source: Arc<Mutex<Weak<Mutex<SourceState>>>> = Arc::default();

        let (input, taking) = (stream.clone(), source.clone());
        let connection = Connection::new(
            config,
            Box::new(move |event| match event {
                connection::Event::AudioPacket(packet) => {
                    input.push(Packet { data: packet.data, time: packet.time, received: packet.received });
                }
                connection::Event::SendEstimate(bps) => estimate(&taking.lock().unwrap(), id, bps),
                connection::Event::KeyframeRequested => keyframe(&taking.lock().unwrap()),
                connection::Event::State(state) => {
                    // What was sent before the connection was up went nowhere — the keyframe asked
                    // for when the source was attached among it. The first picture starts here.
                    if state == State::Connected {
                        fresh_keyframe(&taking.lock().unwrap());
                    }
                    sink(Event::State(state))
                }
                connection::Event::LocalCandidate(line) => sink(Event::LocalCandidate(line)),
                connection::Event::IceRestartNeeded => sink(Event::IceRestartNeeded),
                connection::Event::RemoteMedia { media, active } => sink(Event::RemoteMedia { media, active }),
                connection::Event::VideoFrame(frame) => sink(Event::VideoFrame(frame)),
                connection::Event::ChannelOpen { id, label } => sink(Event::ChannelOpen { id, label }),
                connection::Event::ChannelMessage { id, data, binary } => sink(Event::ChannelMessage { id, data, binary }),
                connection::Event::ChannelBufferedLow { id } => sink(Event::ChannelBufferedLow { id }),
                connection::Event::ChannelClosed { id } => sink(Event::ChannelClosed { id }),
            }),
        )?;
        self.peers.lock().unwrap().push((id, connection.media()));
        Ok(Peer {
            id,
            context: self.clone(),
            connection: Some(connection),
            stream,
            microphone: Mutex::new(None),
            source,
            video: Mutex::new(None),
        })
    }
}

/// The numbers of a peer: the connection's, and the delay its sound is played with.
#[derive(Debug, Clone, Copy)]
pub struct Stats {
    pub connection: connection::Stats,
    pub audio_jitter_buffer_ms: u32,
}

pub struct Peer {
    id: u64,
    context: Context,
    connection: Option<Connection>,
    stream: StreamInput,
    /// The engine's taker while this peer has the microphone.
    microphone: Mutex<Option<u64>>,
    /// The source this peer's network feedback goes to.
    source: Arc<Mutex<Weak<Mutex<SourceState>>>>,
    video: Mutex<Option<VideoSource>>,
}

impl Peer {
    fn connection(&self) -> &Connection {
        self.connection.as_ref().expect("alive until dropped")
    }

    pub fn offer(&self, ice_restart: bool) -> Result<String, Error> {
        self.connection().offer(ice_restart)
    }
    pub fn accept_offer(&self, sdp: &str) -> Result<String, Error> {
        self.connection().accept_offer(sdp)
    }
    pub fn accept_answer(&self, sdp: &str) -> Result<(), Error> {
        self.connection().accept_answer(sdp)
    }
    pub fn add_candidate(&self, candidate: &str) {
        self.connection().add_candidate(candidate)
    }

    /// The microphone, to this peer.
    pub fn send_audio(&self, on: bool) {
        let mut microphone = self.microphone.lock().unwrap();
        if let Some(taker) = microphone.take() {
            self.context.engine.remove_taker(taker);
        }
        if on {
            let media = self.connection().media();
            *microphone = Some(self.context.engine.add_taker(move |frame| {
                media.send_audio(AudioOut { data: frame.data.clone(), time: frame.time, captured: frame.captured });
            }));
        }
    }

    /// A video source, to this peer; None = none.
    pub fn send_video(&self, source: Option<&VideoSource>) {
        let mut video = self.video.lock().unwrap();
        if let Some(before) = video.take() {
            before.detach(self.id);
        }
        *self.source.lock().unwrap() = Weak::new();
        if let Some(source) = source {
            *self.source.lock().unwrap() = Arc::downgrade(&source.state);
            source.attach(self.id, self.connection().media());
            *video = Some(source.clone());
        }
    }

    /// The id of this peer's stream in the engine.
    pub fn stream_id(&self) -> u64 {
        self.stream.id
    }

    /// Leave this peer's sound out of the mix.
    pub fn mute_playout(&self, muted: bool) {
        self.context.engine.set_muted(self.stream.id, muted);
    }

    pub fn request_keyframe(&self) {
        self.connection().request_keyframe()
    }

    pub fn channel_open(&self, label: &str, options: ChannelOptions) -> Result<u32, Error> {
        self.connection().channel_open(label, options)
    }
    pub fn channel_send(&self, id: u32, data: Vec<u8>, binary: bool) -> Result<(), Error> {
        self.connection().channel_send(id, data, binary)
    }
    pub fn channel_buffered(&self, id: u32) -> Result<u64, Error> {
        self.connection().channel_buffered(id)
    }
    pub fn channel_set_buffered_low(&self, id: u32, threshold: u64) {
        self.connection().channel_set_buffered_low(id, threshold)
    }
    pub fn channel_close(&self, id: u32) {
        self.connection().channel_close(id)
    }

    /// How long the sound of this peer waits before it is played: a picture shown that much after
    /// it arrived stays in step with it.
    pub fn playout_delay(&self) -> Duration {
        let level = self.context.engine.stream_stats(self.stream.id).map_or(0, |stats| stats.level_ms);
        Duration::from_millis(level as u64)
    }

    pub fn stats(&self) -> Result<Stats, Error> {
        let connection = self.connection().stats()?;
        let audio_jitter_buffer_ms = self.context.engine.stream_stats(self.stream.id).map_or(0, |stats| stats.level_ms);
        Ok(Stats { connection, audio_jitter_buffer_ms })
    }
}

impl Drop for Peer {
    /// Out of every fan-out first, so nothing is handed to a connection that is going; then the
    /// connection closes and its threads are joined: no event after this returns.
    fn drop(&mut self) {
        self.send_audio(false);
        self.send_video(None);
        self.context.peers.lock().unwrap().retain(|(id, _)| *id != self.id);
        self.connection = None;
        self.context.engine.remove_stream(self.stream.id);
    }
}
