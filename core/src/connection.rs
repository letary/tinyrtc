//! One peer. A connection is a str0m `Rtc`, one UDP socket and two threads:
//!
//!   the READER blocks on the socket and posts every datagram into the connection's queue;
//!   the DRIVER owns the `Rtc` and is the only code that touches it. It sleeps on the queue until
//!   the next str0m deadline, takes ONE message (a datagram, a call of the API, the deadline),
//!   applies it, and polls str0m dry — the rule str0m is built around: one mutation, then
//!   `poll_output` until it answers with a timeout.
//!
//! So the public `Connection` is only a sender into that queue: every method is callable from any
//! thread, and the ones that return a value wait for the driver's reply. Events go out through the
//! sink the connection was made with, on the driver thread.
//!
//! The control channel (`CONTROL_LABEL`) also carries what each side is sending: "video:0" when
//! its camera stops, "video:1" when frames flow again, the same for "audio". Nothing in RTP says "I
//! stopped on purpose" — a receiver can only wait for a timeout, and probing padding on the video
//! line defeats even that — so the sender says it. A peer that never says anything (a browser, a
//! media server) is read by the timeout as before.
//!
//! Relay only (`Config::relay_only`) is enforced here, not left to the app: no address of ours goes
//! into a description or an event, no remote candidate but a `typ relay` one is taken (from
//! `add_candidate` or from a description), and no datagram but a relay's (or a STUN server's
//! answer) reaches str0m. So the other side cannot learn our address from our checks either.
//!
//! Negotiation is the profile's (profile.rs): the first offer always holds audio, video and the
//! data line, so nothing a call does later — a camera turned on, a channel opened — needs another
//! offer. The one exception is an ICE restart.
//!
//! The descriptions are the profile's own SDP (sdp.rs); str0m is told what they agreed through its
//! Direct API, never its SDP API (which, with its parser library, stays out of the binary). str0m
//! fixes the payload types and the header extension ids when an `Rtc` is built, and the Direct API
//! cannot change them afterwards. So:
//!   the OFFERING side keeps the `Rtc` made with the connection, built with the profile's numbers
//!     (they are what its offer says); the answer must keep them;
//!   the ANSWERING side replaces it, at the first offer it takes, with one built with the offer's
//!     numbers. Nothing of the first `Rtc` was seen but its ICE credentials (in the candidates it
//!     trickled) and its candidates: the new one is given the same credentials, the same local
//!     candidates, the remote candidates added so far, the desired bitrate. Channels asked for
//!     before anything was agreed wait in a queue for the data line either way.
//! Once agreed, the numbers stay: a later offer (an ICE restart, either way) must repeat them.
//! What the Direct API cannot do, and so this side does not either: narrow what it sends to an
//! answer that leaves out an extension or transport-cc (Chrome and str0m answer all of ours); take
//! the other side's a=max-message-size (`start_sctp` has no such argument: a message is at most
//! str0m's default 64 KiB); flush the old pairs in an ICE restart (new credentials are set both
//! ways and the paths are checked with them; str0m's SDP API also clears the candidates).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use str0m::bwe::{Bitrate, BweKind};
use str0m::channel::{ChannelConfig, ChannelId, Reliability};
use str0m::config::Fingerprint;
use str0m::format::Codec;
use str0m::media::{KeyframeRequestKind, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::rtp::VideoOrientation;
use str0m::{Candidate, CandidateKind, Event as RtcEvent, IceConnectionState, IceCreds, Input, Output, Rtc};

use crate::profile;
use crate::sdp::{self, Kind, Setup};
use crate::stun::{self, Binding, Scheme, Timing};

/// The label of the channel every connection opens for itself: it is what puts the data line into
/// the first offer, so the app's channels open in-band later. Never shown to the app; a browser
/// peer sees a channel of this name and may ignore it.
pub const CONTROL_LABEL: &str = "tinyrtc";

/// A path lost for this long is a failed connection.
const FAIL_AFTER: Duration = Duration::from_secs(30);
/// A path lost for this long asks for an ICE restart (a blip shorter than it heals by itself).
const RESTART_AFTER: Duration = Duration::from_secs(2);
/// A kind of media not sent for this long is told to the other side as stopped (a camera turned
/// off, a muted microphone). Longer than any gap of a live stream: a low frame rate, Opus's
/// comfort-noise interval.
const QUIET_AFTER: Duration = Duration::from_millis(600);
/// At most one keyframe request per this long, towards the other side.
const KEYFRAME_REQUEST_EVERY: Duration = Duration::from_secs(1);
/// A STUN server is asked 5 times over ~5 s (250 ms, doubling): a call setup does not wait the
/// 39 s of the RFC for an address it can start without.
const STUN_TIMING: Timing = Timing { rto: Duration::from_millis(250), rc: 5, rm: 4 };
/// What our SCTP takes in one message (str0m's default, `RtcConfig::sctp_max_message_size`): the
/// a=max-message-size of our descriptions.
pub(crate) const MAX_MESSAGE_SIZE: u32 = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Media {
    Audio,
    Video,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Malformed input.
    Argument,
    /// Not now: an answer with no offer pending, a closed connection, a channel not open yet.
    State,
    /// The description does not parse or cannot be applied.
    Sdp,
    /// An unknown channel id.
    NotFound,
    /// The connection is closed or failed: nothing will work on it again.
    Closed,
}

/// One picture from the other side: a whole access unit, Annex B.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub data: Arc<[u8]>,
    pub keyframe: bool,
    /// Degrees clockwise to show it upright.
    pub rotation: u16,
    pub rtp_timestamp: u32,
    pub received: Instant,
}

/// One Opus packet from the other side, for the jitter buffer.
#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub data: Arc<[u8]>,
    /// The RTP time extended over its wraps, in 48 kHz samples.
    pub time: u64,
    /// False = something before it was lost.
    pub contiguous: bool,
    pub received: Instant,
}

#[derive(Debug, Clone)]
pub enum Event {
    State(State),
    /// The `candidate:` line of a local candidate found after the description was made.
    LocalCandidate(String),
    IceRestartNeeded,
    RemoteMedia { media: Media, active: bool },
    VideoFrame(VideoFrame),
    AudioPacket(AudioPacket),
    /// The other side lost the picture and wants a keyframe.
    KeyframeRequested,
    /// What the network is believed to carry towards the other side, bits per second.
    SendEstimate(u64),
    ChannelOpen { id: u32, label: String },
    ChannelMessage { id: u32, data: Vec<u8>, binary: bool },
    ChannelBufferedLow { id: u32 },
    ChannelClosed { id: u32 },
}

#[derive(Debug, Clone, Default)]
pub struct IceServer {
    pub url: String,
    pub username: Option<String>,
    pub credential: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub ice_servers: Vec<IceServer>,
    /// Never reveal an address of ours: the only path is a relay (the media server's blind relay
    /// of a call), whatever candidates the app passes. Both halves of it:
    ///   ours are never shown: the connection still has its host (and reflexive) candidates — it
    ///     sends from them — but no description it makes carries an `a=candidate` line (an ICE
    ///     restart's neither) and no `LocalCandidate` event is emitted;
    ///   theirs are never checked: of the other side's candidates — added, or in a description —
    ///     only the `typ relay` ones are taken, every other kind is ignored; and a datagram from
    ///     an address that is not such a relay (or a STUN server answering) is dropped, so no
    ///     peer-reflexive path forms either. Without a relay candidate it never connects.
    /// What still sees the address: the relay (it forwards from it) and the STUN servers of
    /// `ice_servers` (they are asked; what they answer is never shown).
    pub relay_only: bool,
    /// The address to bind and to offer as the host candidate. None = every interface, offering
    /// the address the OS routes outward with.
    pub bind: Option<IpAddr>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ChannelOptions {
    pub unordered: bool,
    pub max_retransmits: Option<u16>,
    pub max_packet_life_ms: Option<u16>,
}

/// One encoded access unit to send, in the packetizer's framing (Annex B).
#[derive(Debug, Clone)]
pub struct VideoOut {
    pub data: Arc<[u8]>,
    pub captured: Instant,
    pub rotation: u16,
}

/// One Opus packet to send.
#[derive(Debug, Clone)]
pub struct AudioOut {
    pub data: Arc<[u8]>,
    /// The position of its first sample in the capture stream, 48 kHz.
    pub time: u64,
    pub captured: Instant,
}

/// The numbers of a connection (the C face's `tinyrtc_stats`, minus what the audio engine adds).
#[derive(Debug, Clone, Copy)]
pub struct Stats {
    pub state: State,
    pub relayed: bool,
    pub rtt_ms: u32,
    pub send_estimate_bps: u32,
    pub audio_send_bps: u32,
    pub audio_recv_bps: u32,
    pub video_send_bps: u32,
    pub video_recv_bps: u32,
    pub audio_loss: f32,
    pub video_loss: f32,
    pub video_recv_fps: u32,
    pub video_nacks_sent: u32,
    pub video_keyframes_requested: u32,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

type Reply<T> = SyncSender<Result<T, Error>>;

enum Command {
    Datagram { from: SocketAddr, data: Vec<u8> },
    /// A STUN server of the configuration, resolved to an address.
    StunServer(SocketAddr),
    Offer { ice_restart: bool, reply: Reply<String> },
    AcceptOffer { sdp: String, reply: Reply<String> },
    AcceptAnswer { sdp: String, reply: Reply<()> },
    AddCandidate(String),
    Video(VideoOut),
    Audio(AudioOut),
    RequestKeyframe,
    DesiredBitrate(u64),
    ChannelOpen { label: String, options: ChannelOptions, reply: Reply<u32> },
    ChannelSend { id: u32, data: Vec<u8>, binary: bool, reply: Option<Reply<()>> },
    ChannelBuffered { id: u32, reply: Reply<u64> },
    ChannelBufferedLow { id: u32, threshold: u64 },
    ChannelClose { id: u32 },
    Stats { reply: Reply<Stats> },
    NetworkChanged,
    Close,
}

pub struct Connection {
    tx: Sender<Command>,
    stop: Arc<AtomicBool>,
    driver: Option<JoinHandle<()>>,
    reader: Option<JoinHandle<()>>,
}

impl Connection {
    /// `sink` gets every event, on the connection's driver thread.
    pub fn new(config: Config, sink: Box<dyn FnMut(Event) + Send>) -> std::io::Result<Connection> {
        let bind_ip = config.bind.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        let socket = UdpSocket::bind(SocketAddr::new(bind_ip, 0))?;
        let port = socket.local_addr()?.port();
        let host = SocketAddr::new(config.bind.unwrap_or_else(outward_ip), port);

        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));

        let reader = {
            let socket = socket.try_clone()?;
            let (tx, stop) = (tx.clone(), stop.clone());
            thread::Builder::new().name("tinyrtc-read".into()).spawn(move || read(socket, tx, stop))?
        };
        resolve_stun_servers(&config.ice_servers, &tx);
        let driver = {
            let (wildcard, relay_only) = (config.bind.is_none(), config.relay_only);
            thread::Builder::new()
                .name("tinyrtc-conn".into())
                .spawn(move || Driver::new(socket, host, wildcard, relay_only, sink).run(rx))?
        };
        Ok(Connection { tx, stop, driver: Some(driver), reader: Some(reader) })
    }

    fn ask<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T, Error> {
        let (reply, answer) = mpsc::sync_channel(1);
        self.tx.send(make(reply)).map_err(|_| Error::State)?;
        answer.recv().map_err(|_| Error::State)?
    }

    fn tell(&self, command: Command) {
        let _ = self.tx.send(command);
    }

    /// The offering side. `ice_restart` for every offer after the first.
    pub fn offer(&self, ice_restart: bool) -> Result<String, Error> {
        self.ask(|reply| Command::Offer { ice_restart, reply })
    }
    /// The answering side: takes the offer, returns the answer.
    pub fn accept_offer(&self, sdp: &str) -> Result<String, Error> {
        self.ask(|reply| Command::AcceptOffer { sdp: sdp.to_owned(), reply })
    }
    /// The offering side: takes the answer.
    pub fn accept_answer(&self, sdp: &str) -> Result<(), Error> {
        self.ask(|reply| Command::AcceptAnswer { sdp: sdp.to_owned(), reply })
    }
    /// A candidate of the other side (its `candidate:` line).
    pub fn add_candidate(&self, candidate: &str) {
        self.tell(Command::AddCandidate(candidate.to_owned()));
    }

    pub fn send_video(&self, frame: VideoOut) {
        self.tell(Command::Video(frame));
    }
    pub fn send_audio(&self, packet: AudioOut) {
        self.tell(Command::Audio(packet));
    }
    /// The decoder cannot go on: ask the other side for a keyframe.
    pub fn request_keyframe(&self) {
        self.tell(Command::RequestKeyframe);
    }
    /// What this side would like to send at when the network allows: the estimate probes up to it.
    pub fn set_desired_bitrate(&self, bps: u64) {
        self.tell(Command::DesiredBitrate(bps));
    }

    /// The id is valid at once; `Event::ChannelOpen` says when the channel can send.
    pub fn channel_open(&self, label: &str, options: ChannelOptions) -> Result<u32, Error> {
        self.ask(|reply| Command::ChannelOpen { label: label.to_owned(), options, reply })
    }
    pub fn channel_send(&self, id: u32, data: Vec<u8>, binary: bool) -> Result<(), Error> {
        self.ask(|reply| Command::ChannelSend { id, data, binary, reply: Some(reply) })
    }
    pub fn channel_buffered(&self, id: u32) -> Result<u64, Error> {
        self.ask(|reply| Command::ChannelBuffered { id, reply })
    }
    pub fn channel_set_buffered_low(&self, id: u32, threshold: u64) {
        self.tell(Command::ChannelBufferedLow { id, threshold });
    }
    pub fn channel_close(&self, id: u32) {
        self.tell(Command::ChannelClose { id });
    }

    pub fn stats(&self) -> Result<Stats, Error> {
        self.ask(|reply| Command::Stats { reply })
    }
    /// The OS reported a change of network.
    pub fn network_changed(&self) {
        self.tell(Command::NetworkChanged);
    }
}

impl Drop for Connection {
    /// Closes towards the other side and waits for both threads: no event after this returns.
    fn drop(&mut self) {
        self.tell(Command::Close);
        if let Some(driver) = self.driver.take() {
            let _ = driver.join();
        }
        self.stop.store(true, Ordering::Relaxed);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// The address the OS would send from towards the internet (no packet is sent to find it);
/// loopback where there is no route at all.
fn outward_ip() -> IpAddr {
    let probe = || -> std::io::Result<IpAddr> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9))?;
        Ok(socket.local_addr()?.ip())
    };
    match probe() {
        Ok(ip) if !ip.is_unspecified() => ip,
        _ => IpAddr::V4(Ipv4Addr::LOCALHOST),
    }
}

/// Names are resolved off the driver thread (the resolver blocks, for seconds on a bad network);
/// every server found is posted to it. IPv4 only, as the socket is.
fn resolve_stun_servers(servers: &[IceServer], tx: &Sender<Command>) {
    let hosts: Vec<(String, u16)> = servers
        .iter()
        .filter_map(|server| stun::parse_ice_url(&server.url).ok())
        .filter(|url| url.scheme == Scheme::Stun)
        .map(|url| (url.host, url.port))
        .collect();
    if hosts.is_empty() {
        return;
    }
    let tx = tx.clone();
    let _ = thread::Builder::new().name("tinyrtc-dns".into()).spawn(move || {
        use std::net::ToSocketAddrs;
        for (host, port) in hosts {
            let found = (host.as_str(), port).to_socket_addrs().ok().and_then(|mut all| all.find(SocketAddr::is_ipv4));
            if let Some(addr) = found {
                if tx.send(Command::StunServer(addr)).is_err() {
                    return;
                }
            }
        }
    });
}

fn read(socket: UdpSocket, tx: Sender<Command>, stop: Arc<AtomicBool>) {
    // The timeout is only how often `stop` is looked at.
    let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
    let mut buf = [0u8; 2000];
    while !stop.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((n, from)) => {
                if tx.send(Command::Datagram { from, data: buf[..n].to_vec() }).is_err() {
                    return;
                }
            }
            Err(e) => match e.kind() {
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {}
                // Windows reports an ICMP "port unreachable" of an earlier send here.
                std::io::ErrorKind::ConnectionReset => {}
                _ => thread::sleep(Duration::from_millis(10)),
            },
        }
    }
}

/// A byte counter turned into a rate between two stats reports.
#[derive(Default, Clone, Copy)]
struct Rate {
    bytes: u64,
    at: Option<Instant>,
    bps: u32,
}

impl Rate {
    fn update(&mut self, bytes: u64, at: Instant) {
        if let Some(before) = self.at {
            let secs = at.saturating_duration_since(before).as_secs_f64();
            if secs > 0.0 {
                self.bps = (bytes.saturating_sub(self.bytes) as f64 * 8.0 / secs) as u32;
            }
        }
        self.bytes = bytes;
        self.at = Some(at);
    }
}

struct Driver {
    rtc: Rtc,
    socket: UdpSocket,
    /// The host candidate: what str0m is told a datagram arrived on.
    host: SocketAddr,
    /// Bound to every interface: the host address can change under the socket.
    wildcard: bool,
    /// `Config::relay_only`: our candidates are used, never shown; of theirs only a relay's are.
    hidden: bool,
    /// Relay only: the addresses of the relay candidates taken — the only ones listened to.
    relays: Vec<SocketAddr>,
    sink: Box<dyn FnMut(Event) + Send>,
    epoch: Instant,

    state: State,
    /// DTLS is up (str0m's `Connected`), whatever ICE says at the moment.
    secured: bool,
    ice: IceConnectionState,
    lost_at: Option<Instant>,
    restart_asked: bool,
    /// The `Rtc` is finished: nothing may touch it any more.
    dead: bool,

    audio: Option<Mid>,
    video: Option<Mid>,
    /// What our descriptions say: made with the first offer or answer.
    session: Option<sdp::Session>,
    /// The other side agreed to it: an answer was made or taken; DTLS and SCTP are started.
    agreed: bool,
    /// Our offer waits for its answer, with the new credentials of the ICE restart it asks for.
    pending: Option<Option<IceCreds>>,
    remote_creds: Option<IceCreds>,
    /// This side is the DTLS client.
    dtls_active: bool,
    /// [audio, video]: the line is agreed and the other side takes what we send on it.
    sends: [bool; 2],
    remote_active: [bool; 2],

    channels: HashMap<u32, ChannelId>,
    channel_ids: HashMap<ChannelId, u32>,
    /// Asked for before anything was agreed: made when the data line is.
    queued_channels: Vec<(u32, ChannelConfig)>,
    control: Option<ChannelId>,
    next_channel: u32,

    /// The questions to STUN servers still open, and the reflexive addresses already offered.
    bindings: Vec<Binding>,
    reflexive: Vec<SocketAddr>,
    /// The local candidates the `Rtc` took (host, reflexive), in order: what a description lists,
    /// and what an `Rtc` built for an offer is given again.
    locals: Vec<Candidate>,
    /// Remote candidates added before anything was agreed, for the same reason.
    early_remote: Vec<Candidate>,
    /// The last `set_desired_bitrate`, for the same reason.
    desired: Option<u64>,

    /// When each kind was last sent, and what the other side was last told about it.
    sent_at: [Option<Instant>; 2],
    told: [bool; 2],
    /// The other side tells what it sends: its word replaces the guessing.
    remote_tells: bool,

    last_keyframe_request: Option<Instant>,
    /// A frame was lost: frames are withheld until a keyframe arrives.
    video_broken: bool,
    stats: Stats,
    rates: [[Rate; 2]; 2], // [audio, video][send, recv]
    frames: u32,
    frames_at: Instant,
}

impl Driver {
    fn new(
        socket: UdpSocket,
        host: SocketAddr,
        wildcard: bool,
        relay_only: bool,
        sink: Box<dyn FnMut(Event) + Send>,
    ) -> Driver {
        let now = Instant::now();
        let mut rtc = profile::config().build(now);
        // Relay only too: the host candidate is what str0m sends from; it is only never shown.
        let mut locals = Vec::new();
        if let Ok(candidate) = Candidate::host(host, "udp") {
            locals.extend(rtc.add_local_candidate(candidate).cloned());
        }
        Driver {
            rtc,
            socket,
            host,
            wildcard,
            hidden: relay_only,
            relays: Vec::new(),
            sink,
            epoch: now,
            state: State::New,
            secured: false,
            ice: IceConnectionState::New,
            lost_at: None,
            restart_asked: false,
            dead: false,
            audio: None,
            video: None,
            session: None,
            agreed: false,
            pending: None,
            remote_creds: None,
            dtls_active: false,
            sends: [false; 2],
            remote_active: [false; 2],
            channels: HashMap::new(),
            channel_ids: HashMap::new(),
            queued_channels: Vec::new(),
            control: None,
            next_channel: 1,
            bindings: Vec::new(),
            reflexive: Vec::new(),
            locals,
            early_remote: Vec::new(),
            desired: None,
            sent_at: [None; 2],
            told: [false; 2],
            remote_tells: false,
            last_keyframe_request: None,
            video_broken: false,
            stats: Stats {
                state: State::New,
                relayed: false,
                rtt_ms: 0,
                send_estimate_bps: 0,
                audio_send_bps: 0,
                audio_recv_bps: 0,
                video_send_bps: 0,
                video_recv_bps: 0,
                audio_loss: 0.0,
                video_loss: 0.0,
                video_recv_fps: 0,
                video_nacks_sent: 0,
                video_keyframes_requested: 0,
                bytes_sent: 0,
                bytes_received: 0,
            },
            rates: [[Rate::default(); 2]; 2],
            frames: 0,
            frames_at: now,
        }
    }

    fn run(mut self, rx: Receiver<Command>) {
        loop {
            let deadline = self.drain();
            let wait = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait) {
                Ok(Command::Close) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(command) => self.command(command),
                Err(RecvTimeoutError::Timeout) => self.tick(),
            }
        }
        // `close` says goodbye (RTCP BYE, the SCTP shutdown, DTLS close_notify) and the poll sends
        // it; `disconnect` would only go silent, and the other side would wait out a timeout.
        if !self.dead && self.rtc.close().is_ok() {
            self.drain();
        }
        self.set_state(State::Closed);
    }

    /// Polls str0m dry; returns when the driver must wake next.
    fn drain(&mut self) -> Instant {
        let idle = Instant::now() + Duration::from_secs(1);
        if self.dead {
            return idle;
        }
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(at)) => return self.own_deadline().map_or(at, |own| own.min(at)),
                Ok(Output::Transmit(t)) => {
                    let _ = self.socket.send_to(&t.contents, t.destination);
                }
                Ok(Output::Event(event)) => self.event(event),
                // One undecodable packet is an error of that packet (str0m dropped its frame and
                // goes on); only an `Rtc` that says it is finished is finished.
                Err(_) if self.rtc.is_alive() => {}
                Err(_) => {
                    self.dead = true;
                    self.set_state(State::Failed);
                    return idle;
                }
            }
        }
    }

    /// The driver's own timers: the restart request, the failure of a lost path, the next send
    /// to a STUN server.
    fn own_deadline(&self) -> Option<Instant> {
        let lost = self.lost_at.map(|lost| if self.restart_asked { lost + FAIL_AFTER } else { lost + RESTART_AFTER });
        let stun = self.bindings.iter().filter_map(Binding::poll_timeout).min();
        let quiet = (0..2).filter(|m| self.told[*m]).filter_map(|m| self.sent_at[m]).map(|at| at + QUIET_AFTER).min();
        [lost, stun, quiet].into_iter().flatten().min()
    }

    /// Tells the other side that a kind of media started or stopped. Not told while the control
    /// channel is not open: `told` stays as it was, and the next frame tries again.
    fn tell(&mut self, media: Media, on: bool) {
        let Some(control) = self.control else { return };
        self.drain();
        let Some(mut channel) = self.rtc.channel(control) else { return };
        let kind = if media == Media::Audio { "audio" } else { "video" };
        let line = format!("{kind}:{}", on as u8);
        if channel.write(false, line.as_bytes()).is_ok() {
            self.told[media as usize] = on;
        }
    }

    /// One frame or packet of `media` went out.
    fn sent(&mut self, media: Media) {
        self.sent_at[media as usize] = Some(Instant::now());
        if !self.told[media as usize] {
            self.tell(media, true);
        }
    }

    /// The other side's word about what it sends.
    fn told_by_remote(&mut self, line: &[u8]) {
        let (media, on) = match line {
            b"audio:0" => (Media::Audio, false),
            b"audio:1" => (Media::Audio, true),
            b"video:0" => (Media::Video, false),
            b"video:1" => (Media::Video, true),
            _ => return,
        };
        self.remote_tells = true;
        self.remote_media(media, on);
    }

    /// Media arrived or a stream timed out: the guess about a peer that does not tell.
    fn remote_media_guess(&mut self, media: Media, active: bool) {
        if !self.remote_tells {
            self.remote_media(media, active);
        }
    }

    /// Sends what the open STUN questions have due, and offers every address they brought.
    fn gather(&mut self, now: Instant) {
        let mut found = Vec::new();
        for binding in &mut self.bindings {
            let server = binding.server();
            if let Some(request) = binding.poll_transmit(now) {
                let _ = self.socket.send_to(request, server);
            }
            if let stun::State::Done(addr) = binding.state() {
                found.push(addr);
            }
        }
        self.bindings.retain(|binding| matches!(binding.state(), stun::State::InProgress));
        for addr in found {
            // Behind no NAT the server sees the host address: nothing new to offer.
            if addr == self.host || self.reflexive.contains(&addr) {
                continue;
            }
            self.reflexive.push(addr);
            let Ok(candidate) = Candidate::server_reflexive(addr, self.host, "udp") else { continue };
            if let Some(added) = self.rtc.add_local_candidate(candidate) {
                let line = added.to_sdp_string();
                self.locals.push(added.clone());
                self.reveal(line);
            }
            self.drain();
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if self.dead {
            return;
        }
        if let Some(lost) = self.lost_at {
            if now >= lost + FAIL_AFTER {
                self.set_state(State::Failed);
                self.lost_at = None;
            } else if !self.restart_asked && now >= lost + RESTART_AFTER {
                self.restart_asked = true;
                (self.sink)(Event::IceRestartNeeded);
            }
        }
        self.gather(now);
        for media in [Media::Audio, Media::Video] {
            let quiet = self.sent_at[media as usize].is_some_and(|at| now >= at + QUIET_AFTER);
            if self.told[media as usize] && quiet {
                self.tell(media, false);
            }
        }
        self.drain();
        if self.rtc.handle_input(Input::Timeout(now)).is_err() {
            self.dead = true;
            self.set_state(State::Failed);
        }
    }

    fn set_state(&mut self, state: State) {
        if self.state != state && self.state != State::Closed {
            self.state = state;
            self.stats.state = state;
            (self.sink)(Event::State(state));
        }
    }

    /// The one state of the C face out of str0m's two (ICE, DTLS).
    fn settle_state(&mut self) {
        use IceConnectionState::*;
        match self.ice {
            Connected | Completed => {
                self.lost_at = None;
                self.restart_asked = false;
                self.set_state(if self.secured { State::Connected } else { State::Connecting });
            }
            Disconnected => {
                if self.lost_at.is_none() {
                    self.lost_at = Some(Instant::now());
                }
                self.set_state(State::Disconnected);
            }
            Checking => {
                if self.state == State::New {
                    self.set_state(State::Connecting);
                }
            }
            New => {}
        }
    }

    fn media_of(&self, mid: Mid) -> Option<Media> {
        if Some(mid) == self.audio {
            Some(Media::Audio)
        } else if Some(mid) == self.video {
            Some(Media::Video)
        } else {
            None
        }
    }

    fn remote_media(&mut self, media: Media, active: bool) {
        let slot = &mut self.remote_active[media as usize];
        if *slot != active {
            *slot = active;
            (self.sink)(Event::RemoteMedia { media, active });
        }
    }

    fn event(&mut self, event: RtcEvent) {
        match event {
            RtcEvent::Connected => {
                self.secured = true;
                self.settle_state();
            }
            RtcEvent::IceConnectionStateChange(ice) => {
                self.ice = ice;
                self.settle_state();
            }
            RtcEvent::MediaAdded(added) => match added.kind {
                MediaKind::Audio => self.audio = self.audio.or(Some(added.mid)),
                MediaKind::Video => self.video = self.video.or(Some(added.mid)),
            },
            RtcEvent::MediaData(data) => match data.params.spec().codec {
                Codec::H264 => {
                    self.remote_media_guess(Media::Video, true);
                    // A picture with a hole before it cannot be decoded right, and a hardware
                    // decoder does not say so: it shows garbage until the next keyframe. So the
                    // stream is cut here — nothing is handed out until a keyframe, which is asked for.
                    let keyframe = data.is_keyframe();
                    if keyframe {
                        self.video_broken = false;
                    } else if !data.contiguous {
                        self.video_broken = true;
                    }
                    if self.video_broken {
                        return self.request_keyframe();
                    }
                    self.frames += 1;
                    let rotation = match data.ext_vals.video_orientation {
                        Some(VideoOrientation::Deg90) => 90,
                        Some(VideoOrientation::Deg180) => 180,
                        Some(VideoOrientation::Deg270) => 270,
                        _ => 0,
                    };
                    (self.sink)(Event::VideoFrame(VideoFrame {
                        keyframe: data.is_keyframe(),
                        rotation,
                        rtp_timestamp: data.time.numer() as u32,
                        received: data.network_time,
                        data: data.data,
                    }));
                }
                Codec::Opus => {
                    self.remote_media_guess(Media::Audio, true);
                    (self.sink)(Event::AudioPacket(AudioPacket {
                        time: data.time.numer(),
                        contiguous: data.contiguous,
                        received: data.network_time,
                        data: data.data,
                    }));
                }
                _ => {}
            },
            RtcEvent::StreamPaused(paused) => {
                if let Some(media) = self.media_of(paused.mid) {
                    self.remote_media_guess(media, !paused.paused);
                }
            }
            RtcEvent::KeyframeRequest(_) => (self.sink)(Event::KeyframeRequested),
            RtcEvent::EgressBitrateEstimate(kind) => {
                let estimate = match kind {
                    BweKind::Twcc { estimate, .. } => estimate,
                    BweKind::Remb { estimate, .. } => estimate,
                    _ => return,
                };
                self.stats.send_estimate_bps = estimate.as_u64().min(u32::MAX as u64) as u32;
                (self.sink)(Event::SendEstimate(estimate.as_u64()));
            }
            RtcEvent::ChannelOpen(channel, label) => {
                if label == CONTROL_LABEL {
                    self.control = Some(channel);
                    // Whatever was said before it opened was not heard: say it again.
                    self.told = [false; 2];
                    return;
                }
                let id = match self.channel_ids.get(&channel) {
                    Some(id) => *id,
                    None => self.adopt_channel(channel),
                };
                (self.sink)(Event::ChannelOpen { id, label });
            }
            RtcEvent::ChannelData(message) => {
                if Some(message.id) == self.control {
                    return self.told_by_remote(&message.data);
                }
                if let Some(id) = self.channel_ids.get(&message.id) {
                    (self.sink)(Event::ChannelMessage { id: *id, data: message.data, binary: message.binary });
                }
            }
            RtcEvent::ChannelBufferedAmountLow(channel) => {
                if let Some(id) = self.channel_ids.get(&channel) {
                    (self.sink)(Event::ChannelBufferedLow { id: *id });
                }
            }
            RtcEvent::ChannelClose(channel) => {
                if let Some(id) = self.channel_ids.remove(&channel) {
                    self.channels.remove(&id);
                    (self.sink)(Event::ChannelClosed { id });
                }
            }
            RtcEvent::PeerStats(peer) => {
                self.stats.bytes_sent = peer.peer_bytes_tx;
                self.stats.bytes_received = peer.peer_bytes_rx;
                if let Some(rtt) = peer.rtt {
                    self.stats.rtt_ms = rtt.as_millis() as u32;
                }
                let secs = peer.timestamp.saturating_duration_since(self.frames_at).as_secs_f64();
                if secs > 0.0 {
                    self.stats.video_recv_fps = (self.frames as f64 / secs).round() as u32;
                }
                self.frames = 0;
                self.frames_at = peer.timestamp;
            }
            RtcEvent::MediaEgressStats(egress) => {
                if let Some(media) = self.media_of(egress.mid) {
                    let rate = &mut self.rates[media as usize][0];
                    rate.update(egress.bytes, egress.timestamp);
                    match media {
                        Media::Audio => self.stats.audio_send_bps = rate.bps,
                        Media::Video => self.stats.video_send_bps = rate.bps,
                    }
                }
            }
            RtcEvent::MediaIngressStats(ingress) => {
                if let Some(media) = self.media_of(ingress.mid) {
                    let rate = &mut self.rates[media as usize][1];
                    rate.update(ingress.bytes, ingress.timestamp);
                    let loss = ingress.loss.unwrap_or(0.0);
                    match media {
                        Media::Audio => {
                            self.stats.audio_recv_bps = rate.bps;
                            self.stats.audio_loss = loss;
                        }
                        Media::Video => {
                            self.stats.video_recv_bps = rate.bps;
                            self.stats.video_loss = loss;
                            self.stats.video_nacks_sent = ingress.nacks as u32;
                        }
                    }
                }
            }
            RtcEvent::Closed => {
                self.dead = true;
                self.set_state(State::Closed);
            }
            _ => {}
        }
    }

    /// A channel the other side opened: give it an id of ours.
    fn adopt_channel(&mut self, channel: ChannelId) -> u32 {
        let id = self.next_channel;
        self.next_channel += 1;
        self.channels.insert(id, channel);
        self.channel_ids.insert(channel, id);
        id
    }

    fn command(&mut self, command: Command) {
        if self.dead {
            return self.refuse(command);
        }
        match command {
            Command::StunServer(server) => {
                let now = Instant::now();
                self.bindings.push(Binding::with_timing(server, now, STUN_TIMING));
                self.gather(now);
            }
            Command::Datagram { from, data } => {
                // The socket is shared with the STUN of str0m itself (the ICE checks): an answer
                // to one of OUR questions is taken here, everything else is for str0m.
                if stun::is_stun(&data) && self.bindings.iter_mut().any(|binding| binding.handle_input(from, &data)) {
                    return self.gather(Instant::now());
                }
                // Relay only: a relay is the only one listened to. A check from anywhere else
                // would make its sender a peer-reflexive candidate — a path that is not the relay.
                if self.hidden && !self.relays.contains(&from) {
                    return;
                }
                let Ok(contents) = data.as_slice().try_into() else { return };
                let input = Input::Receive(
                    Instant::now(),
                    Receive { proto: Protocol::Udp, source: from, destination: self.host, contents },
                );
                // A datagram str0m has no use for is not an error of the connection.
                let _ = self.rtc.handle_input(input);
            }
            Command::Offer { ice_restart, reply } => {
                let _ = reply.send(self.offer(ice_restart));
            }
            Command::AcceptOffer { sdp, reply } => {
                let _ = reply.send(self.accept_offer(&sdp));
            }
            Command::AcceptAnswer { sdp, reply } => {
                let _ = reply.send(self.accept_answer(&sdp));
            }
            Command::AddCandidate(line) => {
                if let Some(candidate) = self.admit(&line) {
                    if !self.agreed {
                        self.early_remote.push(candidate.clone());
                    }
                    self.rtc.add_remote_candidate(candidate);
                }
            }
            Command::Video(frame) => self.send_video(frame),
            Command::Audio(packet) => self.send_audio(packet),
            Command::RequestKeyframe => self.request_keyframe(),
            Command::DesiredBitrate(bps) => {
                self.desired = Some(bps);
                self.rtc.bwe().set_bitrate(Bitrate::ZERO, Bitrate::bps(bps));
            }
            Command::ChannelOpen { label, options, reply } => {
                let _ = reply.send(self.channel_open(label, options));
            }
            Command::ChannelSend { id, data, binary, reply } => {
                let result = self.channel_send(id, &data, binary);
                if let Some(reply) = reply {
                    let _ = reply.send(result);
                }
            }
            Command::ChannelBuffered { id, reply } => {
                let result = match self.channels.get(&id) {
                    None if self.queued_channels.iter().any(|(queued, _)| *queued == id) => Ok(0),
                    None => Err(Error::NotFound),
                    Some(channel) => match self.rtc.channel(*channel) {
                        Some(mut channel) => Ok(channel.buffered_amount() as u64),
                        None => Ok(0),
                    },
                };
                let _ = reply.send(result);
            }
            Command::ChannelBufferedLow { id, threshold } => {
                if let Some(mut channel) = self.channels.get(&id).and_then(|c| self.rtc.channel(*c)) {
                    channel.set_buffered_amount_low_threshold(threshold as usize);
                }
            }
            Command::ChannelClose { id } => {
                // One still waiting for the data line is simply never made.
                self.queued_channels.retain(|(queued, _)| *queued != id);
                if let Some(channel) = self.channels.get(&id) {
                    self.rtc.direct_api().close_data_channel(*channel);
                }
            }
            Command::Stats { reply } => {
                let _ = reply.send(Ok(self.stats));
            }
            Command::NetworkChanged => self.network_changed(),
            Command::Close => {}
        }
    }

    /// A finished connection answers every call that waits, with `Closed`.
    fn refuse(&mut self, command: Command) {
        match command {
            Command::Offer { reply, .. } | Command::AcceptOffer { reply, .. } => {
                let _ = reply.send(Err(Error::Closed));
            }
            Command::AcceptAnswer { reply, .. } => {
                let _ = reply.send(Err(Error::Closed));
            }
            Command::ChannelSend { reply: Some(reply), .. } => {
                let _ = reply.send(Err(Error::Closed));
            }
            Command::ChannelOpen { reply, .. } => {
                let _ = reply.send(Err(Error::Closed));
            }
            Command::ChannelBuffered { reply, .. } => {
                let _ = reply.send(Err(Error::Closed));
            }
            Command::Stats { reply } => {
                let _ = reply.send(Ok(self.stats));
            }
            _ => {}
        }
    }

    /// The first offer, or an ICE restart. Offered again before its answer came, the first offer
    /// is the same offer (with the candidates found since); `ice_restart` means nothing until
    /// something was agreed (the credentials are new anyway).
    fn offer(&mut self, ice_restart: bool) -> Result<String, Error> {
        let restart = match (&self.session, self.agreed) {
            (None, _) => {
                let session = self.new_session();
                self.session = Some(session);
                // The side that offers first controls ICE (we are never ICE-lite).
                self.rtc.direct_api().set_ice_controlling(true);
                None
            }
            (Some(_), false) if ice_restart => None,
            // New credentials; they replace the old ones when the answer comes, as str0m's SDP
            // API does it (until then the path in use keeps working).
            (Some(_), true) if ice_restart => Some(IceCreds::new()),
            // Nothing new to offer.
            (Some(_), _) => return Err(Error::State),
        };
        let text = self.describe(Setup::ActPass, restart.clone())?;
        self.pending = Some(restart);
        if self.state == State::New {
            self.set_state(State::Connecting);
        }
        Ok(text)
    }

    /// Our side of a first offer: the profile's sections and numbers, fresh SSRCs.
    fn new_session(&mut self) -> sdp::Session {
        let token = || sdp::random_token(20);
        let mut session = sdp::Session::offer(sdp::random_u64() >> 2, token(), token(), [token(), token()], MAX_MESSAGE_SIZE);
        let api = self.rtc.direct_api();
        let audio = *api.new_ssrc();
        let (video, rtx) = loop {
            let (video, rtx) = (*api.new_ssrc(), *api.new_ssrc());
            if video != rtx && video != audio && rtx != audio {
                break (video, rtx);
            }
        };
        if let Some(section) = session.section_mut(Kind::Audio) {
            section.ssrc = Some((audio, None));
        }
        if let Some(section) = session.section_mut(Kind::Video) {
            section.ssrc = Some((video, Some(rtx)));
        }
        session
    }

    /// A description of the session: `creds` = new ICE credentials it announces (an ICE
    /// restart), else the ones in use. Relay only: without our candidates.
    fn describe(&mut self, setup: Setup, creds: Option<IceCreds>) -> Result<String, Error> {
        let (creds, fingerprint) = {
            let api = self.rtc.direct_api();
            (creds.unwrap_or_else(|| api.local_ice_credentials()), api.local_dtls_fingerprint().to_string())
        };
        let candidates: Vec<String> = if self.hidden {
            Vec::new()
        } else {
            self.locals
                .iter()
                .map(|c| {
                    let mut c = c.clone();
                    c.set_ufrag(&creds.ufrag);
                    c.to_sdp_string()
                })
                .collect()
        };
        let session = self.session.as_mut().ok_or(Error::State)?;
        let transport =
            sdp::Transport { ufrag: &creds.ufrag, pwd: &creds.pass, fingerprint: &fingerprint, setup, candidates: &candidates };
        Ok(session.write(&transport))
    }

    /// A local candidate found after the description was made, unless relay only hides it.
    fn reveal(&mut self, line: String) {
        if !self.hidden {
            (self.sink)(Event::LocalCandidate(line));
        }
    }

    /// A remote candidate this connection takes, added or in a description: UDP (the socket is);
    /// relay only, the relay type alone — the other side's own addresses are never checked,
    /// whatever the app passes. A name instead of an address (a browser's mDNS candidate) does not
    /// parse: skipped, the path is found through the candidates that do.
    fn admit(&mut self, line: &str) -> Option<Candidate> {
        let candidate = Candidate::from_sdp_string(line).ok().filter(|c| c.proto() == Protocol::Udp)?;
        if self.hidden {
            if candidate.kind() != CandidateKind::Relayed {
                return None;
            }
            if !self.relays.contains(&candidate.addr()) {
                self.relays.push(candidate.addr());
            }
        }
        Some(candidate)
    }

    fn add_remote_candidates(&mut self, lines: &[String]) {
        for line in lines {
            if let Some(candidate) = self.admit(line) {
                self.rtc.add_remote_candidate(candidate);
            }
        }
    }

    /// Replaces the `Rtc` (the answering side, at the offer: see the module comment). The new one
    /// is given what the old one had: the local candidates, the remote candidates so far, the
    /// desired bitrate. Its ICE credentials are set by the caller's config.
    fn install(&mut self, rtc: Rtc) {
        self.rtc = rtc;
        for candidate in std::mem::take(&mut self.locals) {
            if let Some(added) = self.rtc.add_local_candidate(candidate) {
                self.locals.push(added.clone());
            }
        }
        for candidate in &self.early_remote {
            self.rtc.add_remote_candidate(candidate.clone());
        }
        if let Some(bps) = self.desired {
            self.rtc.bwe().set_bitrate(Bitrate::ZERO, Bitrate::bps(bps));
        }
    }

    fn accept_offer(&mut self, text: &str) -> Result<String, Error> {
        let remote = sdp::Remote::parse(text).map_err(|_| Error::Sdp)?;
        let plan = sdp::answer_plan(&remote).map_err(|_| Error::Sdp)?;
        if self.agreed {
            return self.accept_reoffer(plan);
        }
        // An offer of ours still waiting is given up for theirs (str0m's SDP API did the same).
        self.pending = None;
        let creds = self.rtc.direct_api().local_ice_credentials();
        let rtc = profile::config_with(&plan.numbers).set_local_ice_credentials(creds).build(Instant::now());
        self.install(rtc);
        self.audio = None;
        self.video = None;

        let token = || sdp::random_token(20);
        let mut session = sdp::Session {
            id: sdp::random_u64() >> 2,
            version: 1,
            numbers: plan.numbers.clone(),
            sections: plan.sections,
            stream: token(),
            cname: token(),
            max_message_size: MAX_MESSAGE_SIZE,
        };
        // Our SSRCs: none of theirs.
        let mut used: Vec<u32> =
            plan.peers.iter().flatten().filter_map(|p| p.ssrc).flat_map(|(main, rtx)| std::iter::once(main).chain(rtx)).collect();
        {
            let api = self.rtc.direct_api();
            let mut fresh = || loop {
                let ssrc = *api.new_ssrc();
                if !used.contains(&ssrc) {
                    used.push(ssrc);
                    break ssrc;
                }
            };
            for section in session.sections.iter_mut().filter(|s| s.rejected.is_none()) {
                match section.kind {
                    Kind::Audio => section.ssrc = Some((fresh(), None)),
                    Kind::Video => section.ssrc = Some((fresh(), section.rtx.then(&mut fresh))),
                    _ => continue,
                }
                section.track = token();
            }
        }

        let transport = plan.transport;
        let theirs = IceCreds { ufrag: transport.ufrag, pass: transport.pwd };
        // str0m's answer to actpass (and to active) is passive: the offerer then connects.
        let active = transport.setup == Some(Setup::Passive);
        {
            let mut api = self.rtc.direct_api();
            // The offerer controls ICE, unless it is ICE-lite.
            api.set_ice_controlling(transport.ice_lite);
            api.set_remote_ice_credentials(theirs.clone());
            api.set_remote_fingerprint(Fingerprint { hash_func: transport.fingerprint.0, bytes: transport.fingerprint.1 });
            api.start_dtls(active).map_err(|_| Error::Sdp)?;
        }
        self.remote_creds = Some(theirs);
        self.dtls_active = active;
        self.session = Some(session);
        self.agree(&plan.peers, plan.numbers.transport_cc.contains(&true), false);
        self.add_remote_candidates(&plan.candidates);
        if self.state == State::New {
            self.set_state(State::Connecting);
        }
        self.describe(if active { Setup::Active } else { Setup::Passive }, None)
    }

    /// An offer after the session was agreed: an ICE restart (theirs), or the same session again.
    /// The sections and numbers must be the agreed ones; str0m cannot change them.
    fn accept_reoffer(&mut self, plan: sdp::Plan) -> Result<String, Error> {
        let session = self.session.as_mut().ok_or(Error::State)?;
        let (old, new) = (&session.numbers, &plan.numbers);
        let used = |kind: Kind| session.section(kind).is_some();
        let same = plan.sections.len() == session.sections.len()
            && plan
                .sections
                .iter()
                .zip(&session.sections)
                .all(|(n, o)| n.rejected.is_some() == o.rejected.is_some() && (o.rejected.is_some() || (n.kind == o.kind && n.mid == o.mid)))
            && (!used(Kind::Audio) || old.opus == new.opus)
            && (!used(Kind::Video) || (old.h264 == new.h264 && old.h264_rtx == new.h264_rtx));
        if !same {
            return Err(Error::Sdp);
        }
        for (section, offered) in session.sections.iter_mut().zip(&plan.sections) {
            section.direction = offered.direction;
            section.exts.retain(|e| offered.exts.contains(e));
        }
        self.update_media(&plan.peers);
        let theirs = IceCreds { ufrag: plan.transport.ufrag, pass: plan.transport.pwd };
        let mut api = self.rtc.direct_api();
        // New credentials of theirs are an ICE restart: new ones of ours answer it.
        if self.remote_creds.as_ref() != Some(&theirs) {
            api.set_local_ice_credentials(IceCreds::new());
        }
        api.set_remote_ice_credentials(theirs.clone());
        self.remote_creds = Some(theirs);
        self.pending = None;
        self.add_remote_candidates(&plan.candidates);
        self.describe(if self.dtls_active { Setup::Active } else { Setup::Passive }, None)
    }

    fn accept_answer(&mut self, text: &str) -> Result<(), Error> {
        let restart = self.pending.take().ok_or(Error::State)?;
        let remote = sdp::Remote::parse(text).map_err(|_| Error::Sdp)?;
        let session = self.session.as_ref().ok_or(Error::State)?;
        let answer = sdp::read_answer(session, &remote).map_err(|_| Error::Sdp)?;
        let theirs = IceCreds { ufrag: answer.transport.ufrag.clone(), pass: answer.transport.pwd.clone() };
        // New credentials of theirs in an answer accept the ICE restart we offered; one we did not
        // offer breaks the rules (str0m's SDP API refuses it too).
        let changed = self.remote_creds.as_ref().is_some_and(|known| *known != theirs);
        if changed && restart.is_none() {
            return Err(Error::Sdp);
        }
        if !self.agreed {
            // The sections it leaves out are out from now on.
            if let Some(session) = self.session.as_mut() {
                for (section, peer) in session.sections.iter_mut().zip(&answer.peers) {
                    if peer.is_none() && section.rejected.is_none() {
                        section.rejected = Some(sdp::rejected_line(section.kind, &session.numbers));
                    }
                    if section.kind == Kind::Video {
                        section.rtx &= answer.video_rtx;
                    }
                }
            }
        }
        {
            let mut api = self.rtc.direct_api();
            if let (true, Some(ours)) = (changed, restart) {
                api.set_local_ice_credentials(ours);
            }
            api.set_remote_ice_credentials(theirs.clone());
            if !self.agreed {
                let (hash_func, bytes) = answer.transport.fingerprint.clone();
                api.set_remote_fingerprint(Fingerprint { hash_func, bytes });
                // Their passive makes us the DTLS client; active, actpass (not an answer's) or
                // none make us the server, as with str0m.
                let active = answer.transport.setup == Some(Setup::Passive);
                api.start_dtls(active).map_err(|_| Error::Sdp)?;
                self.dtls_active = active;
            }
        }
        self.remote_creds = Some(theirs);
        if !self.agreed {
            self.agree(&answer.peers, answer.transport_cc, true);
        }
        self.add_remote_candidates(&answer.candidates);
        Ok(())
    }

    /// Tells str0m what the first offer and answer agreed: the media lines, what each side sends
    /// (our SSRCs; theirs when their description named them, else str0m finds them by the mid
    /// extension), TWCC feedback, the data line and the channels that waited for it. The side
    /// that `offered` opens the control channel (the other takes it when it opens).
    fn agree(&mut self, peers: &[Option<sdp::Peer>], transport_cc: bool, offered: bool) {
        self.agreed = true;
        self.early_remote.clear();
        let Some(session) = &self.session else { return };
        let twcc = transport_cc && session.numbers.exts.iter().any(|(_, e)| *e == sdp::Ext::TransportCc);
        let data = session.section(Kind::Application).is_some();
        {
            let mut api = self.rtc.direct_api();
            for (section, _) in session.sections.iter().zip(peers).filter(|(s, p)| p.is_some() && s.rejected.is_none()) {
                let kind = match section.kind {
                    Kind::Audio => MediaKind::Audio,
                    Kind::Video => MediaKind::Video,
                    _ => continue,
                };
                let mid = Mid::from(section.mid.as_str());
                api.declare_media(mid, kind);
                match kind {
                    MediaKind::Audio => self.audio = Some(mid),
                    MediaKind::Video => self.video = Some(mid),
                }
            }
            if twcc {
                api.enable_twcc_feedback();
            }
            if data {
                api.start_sctp(self.dtls_active);
            }
        }
        self.update_media(peers);
        if !data {
            return;
        }
        if offered {
            let config = ChannelConfig { label: CONTROL_LABEL.into(), ..Default::default() };
            self.control = Some(self.rtc.direct_api().create_data_channel(config));
        }
        for (id, config) in std::mem::take(&mut self.queued_channels) {
            let channel = self.rtc.direct_api().create_data_channel(config);
            self.channels.insert(id, channel);
            self.channel_ids.insert(channel, id);
        }
    }

    /// Who sends what on each line, as the last description of theirs says: our send streams for
    /// the lines they take, their receive streams for the SSRCs they named.
    fn update_media(&mut self, peers: &[Option<sdp::Peer>]) {
        let Some(session) = &self.session else { return };
        let mut api = self.rtc.direct_api();
        for (section, peer) in session.sections.iter().zip(peers) {
            let (Some(peer), None) = (peer, &section.rejected) else { continue };
            let slot = match section.kind {
                Kind::Audio => Media::Audio as usize,
                Kind::Video => Media::Video as usize,
                _ => continue,
            };
            let mid = Mid::from(section.mid.as_str());
            if api.stream_tx_by_mid(mid, None).is_none() {
                if let Some((ssrc, rtx)) = section.ssrc.filter(|_| section.direction.sends() && peer.receives) {
                    api.declare_stream_tx(ssrc.into(), rtx.filter(|_| section.rtx).map(Into::into), mid, None);
                }
            }
            self.sends[slot] = section.direction.sends() && peer.receives && api.stream_tx_by_mid(mid, None).is_some();
            if let Some((ssrc, rtx)) = peer.ssrc.filter(|_| section.direction.receives()) {
                let ours = session.sections.iter().filter_map(|s| s.ssrc).any(|(m, r)| m == ssrc || r == Some(ssrc));
                if !ours {
                    // As str0m's SDP API: no RTX of theirs, no NACKs to them.
                    api.expect_stream_rx(ssrc.into(), rtx.map(Into::into), mid, None).suppress_nack(rtx.is_none());
                }
            }
        }
    }

    fn send_video(&mut self, frame: VideoOut) {
        let (Some(mid), true, true) = (self.video, self.secured, self.sends[Media::Video as usize]) else { return };
        let Some(pt) = self.session.as_ref().map(|s| s.numbers.h264) else { return };
        let Some(writer) = self.rtc.writer(mid) else { return };
        let micros = frame.captured.saturating_duration_since(self.epoch).as_micros() as u64;
        let time = MediaTime::from_90khz(micros * 9 / 100);
        let orientation = match frame.rotation {
            90 => VideoOrientation::Deg90,
            180 => VideoOrientation::Deg180,
            270 => VideoOrientation::Deg270,
            _ => VideoOrientation::Deg0,
        };
        if writer.video_orientation(orientation).write(pt.into(), frame.captured, time, frame.data).is_ok() {
            self.sent(Media::Video);
        }
    }

    /// Only on a line the other side takes: str0m fails the whole `Rtc` (NoSenderSource) for a
    /// frame written where no send stream was declared.
    fn send_audio(&mut self, packet: AudioOut) {
        let (Some(mid), true, true) = (self.audio, self.secured, self.sends[Media::Audio as usize]) else { return };
        let Some(pt) = self.session.as_ref().map(|s| s.numbers.opus) else { return };
        let Some(writer) = self.rtc.writer(mid) else { return };
        let time = MediaTime::new(packet.time, str0m::media::Frequency::FORTY_EIGHT_KHZ);
        if writer.write(pt.into(), packet.captured, time, packet.data).is_ok() {
            self.sent(Media::Audio);
        }
    }

    fn request_keyframe(&mut self) {
        let now = Instant::now();
        if self.last_keyframe_request.is_some_and(|at| now < at + KEYFRAME_REQUEST_EVERY) {
            return;
        }
        let Some(mut writer) = self.video.and_then(|mid| self.rtc.writer(mid)) else { return };
        if writer.request_keyframe(None, KeyframeRequestKind::Pli).is_ok() {
            self.last_keyframe_request = Some(now);
            self.stats.video_keyframes_requested += 1;
        }
    }

    fn channel_open(&mut self, label: String, options: ChannelOptions) -> Result<u32, Error> {
        if label == CONTROL_LABEL {
            return Err(Error::Argument);
        }
        let reliability = match (options.max_retransmits, options.max_packet_life_ms) {
            (Some(_), Some(_)) => return Err(Error::Argument),
            (Some(retransmits), None) => Reliability::MaxRetransmits { retransmits },
            (None, Some(lifetime)) => Reliability::MaxPacketLifetime { lifetime },
            (None, None) => Reliability::Reliable,
        };
        let config = ChannelConfig { label, ordered: !options.unordered, reliability, ..Default::default() };
        let id = self.next_channel;
        self.next_channel += 1;
        if self.agreed {
            // The data line is agreed: the channel opens in-band.
            let channel = self.rtc.direct_api().create_data_channel(config);
            self.channels.insert(id, channel);
            self.channel_ids.insert(channel, id);
        } else {
            self.queued_channels.push((id, config));
        }
        Ok(id)
    }

    fn channel_send(&mut self, id: u32, data: &[u8], binary: bool) -> Result<(), Error> {
        let channel = *self.channels.get(&id).ok_or_else(|| {
            if self.queued_channels.iter().any(|(queued, _)| *queued == id) { Error::State } else { Error::NotFound }
        })?;
        let mut channel = self.rtc.channel(channel).ok_or(Error::State)?;
        channel.write(binary, data).map(|_| ()).map_err(|_| Error::State)
    }

    /// The route outward moved to another address (Wi-Fi to cellular): offer it, and ask for the
    /// restart that makes both sides check paths again.
    fn network_changed(&mut self) {
        if !self.wildcard {
            return;
        }
        let ip = outward_ip();
        if ip == self.host.ip() {
            return;
        }
        if let Ok(old) = Candidate::host(self.host, "udp") {
            self.rtc.direct_api().invalidate_candidate(&old);
            self.drain();
        }
        let old = self.host;
        self.locals.retain(|c| !(c.kind() == CandidateKind::Host && c.addr() == old));
        self.host = SocketAddr::new(ip, self.host.port());
        if let Ok(candidate) = Candidate::host(self.host, "udp") {
            if let Some(added) = self.rtc.add_local_candidate(candidate) {
                let line = added.to_sdp_string();
                self.locals.push(added.clone());
                self.reveal(line);
            }
        }
        if self.session.is_some() {
            self.restart_asked = true;
            (self.sink)(Event::IceRestartNeeded);
        }
    }
}

/// The media side of a connection for the fan-outs (the microphone, a video source): a cheap
/// handle that only posts into the connection's queue, and posts into nothing once it is gone.
#[derive(Clone)]
pub struct MediaSender {
    tx: Sender<Command>,
}

impl MediaSender {
    pub fn send_video(&self, frame: VideoOut) {
        let _ = self.tx.send(Command::Video(frame));
    }
    pub fn send_audio(&self, packet: AudioOut) {
        let _ = self.tx.send(Command::Audio(packet));
    }
    pub fn set_desired_bitrate(&self, bps: u64) {
        let _ = self.tx.send(Command::DesiredBitrate(bps));
    }
    pub fn network_changed(&self) {
        let _ = self.tx.send(Command::NetworkChanged);
    }
}

impl Connection {
    pub fn media(&self) -> MediaSender {
        MediaSender { tx: self.tx.clone() }
    }
}
