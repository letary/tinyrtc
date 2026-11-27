//! Interop probe — not the design. One str0m peer restricted to the tinyrtc profile (the same
//! `config()` as core: H.264 + Opus, one data channel, bandwidth estimation on) that a real browser
//! calls. The browser offers (POST /offer with the SDP text, the answer SDP comes back); the peer
//! counts what it receives and echoes it: video and audio go back on the m-line they came in on,
//! every channel message is written back on its channel.
//!
//! stdout carries only JSON lines: one `listening` line, a `stats` line every second, a `final`
//! line on exit (after `--seconds N`). Diagnostics go to stderr (`RUST_LOG=str0m=debug` for str0m).
//!
//!   tinyrtc-peer [--host 127.0.0.1] [--http-port 0] [--udp-port 0] [--seconds 30] [--drop-video N]
//!
//! `--drop-video N` drops every Nth outgoing video RTP packet and every RTX packet, so the
//! browser's NACKs can't repair the echo and it has to ask for a keyframe: the forwarding path.

use std::collections::HashSet;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use str0m::bwe::{Bitrate, BweKind};
use str0m::change::SdpOffer;
use str0m::channel::ChannelId;
use str0m::format::Codec;
use str0m::media::{KeyframeRequestKind, MediaData, MediaKind, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcConfig};

/// Identical to `core/src/lib.rs` `config()`: the profile under test.
/// The profile: Opus, and ONE H.264 variant — Constrained Baseline 3.1, packetization-mode 1
/// (`enable_h264` would register seven, and an offer naming them all then picks another).
fn config() -> RtcConfig {
    let mut config = Rtc::builder()
        .clear_codecs()
        .enable_opus(true, false)
        .enable_bwe(Some(Bitrate::kbps(300)));
    config.codec_config().add_h264(108.into(), Some(109.into()), true, 0x42e01f);
    config
}

struct Args {
    host: IpAddr,
    http_port: u16,
    udp_port: u16,
    seconds: u64,
    drop_video: u32,
}

fn args() -> Args {
    let mut a = Args { host: "127.0.0.1".parse().unwrap(), http_port: 0, udp_port: 0, seconds: 30, drop_video: 0 };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let v = it.next().unwrap_or_else(|| die(&format!("{k} needs a value")));
        match k.as_str() {
            "--host" => a.host = v.parse().unwrap_or_else(|_| die("--host: not an IP address")),
            "--http-port" => a.http_port = v.parse().unwrap_or_else(|_| die("--http-port: not a port")),
            "--udp-port" => a.udp_port = v.parse().unwrap_or_else(|_| die("--udp-port: not a port")),
            "--seconds" => a.seconds = v.parse().unwrap_or_else(|_| die("--seconds: not a number")),
            "--drop-video" => a.drop_video = v.parse().unwrap_or_else(|_| die("--drop-video: not a number")),
            _ => die(&format!("unknown argument {k}")),
        }
    }
    a
}

fn die(msg: &str) -> ! {
    eprintln!("tinyrtc-peer: {msg}");
    std::process::exit(2);
}

/// What the peer saw — the numbers the interop run is judged by.
#[derive(Default)]
struct Counters {
    offers: u32,
    ice: String,
    connected: bool,
    video_codec: String,
    video_frames: u32,
    video_keyframes: u32,
    video_bytes: u64,
    video_gaps: u32,
    video_echoed: u32,
    video_packets_dropped: u32,
    audio_codec: String,
    audio_frames: u32,
    audio_echoed: u32,
    echo_errors: u32,
    keyframe_requests_sent: u32,
    keyframe_requests_failed: u32,
    keyframe_requests_received: u32,
    channel_open: bool,
    channel_label: String,
    channel_messages: u32,
    channel_echoes: u32,
    bwe_kbps: u64,
    send_errors: u32,
    rtc_error: String,
}

impl Counters {
    fn json(&self, kind: &str, t: f64) -> String {
        let s = |v: &str| format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""));
        format!(
            concat!(
                "{{\"type\":{},\"t\":{:.1},\"offers\":{},\"ice\":{},\"connected\":{},",
                "\"video_codec\":{},\"video_frames\":{},\"video_keyframes\":{},\"video_bytes\":{},",
                "\"video_gaps\":{},\"video_echoed\":{},\"video_packets_dropped\":{},\"audio_codec\":{},\"audio_frames\":{},",
                "\"audio_echoed\":{},\"echo_errors\":{},\"keyframe_requests_sent\":{},",
                "\"keyframe_requests_failed\":{},\"keyframe_requests_received\":{},",
                "\"channel_open\":{},\"channel_label\":{},\"channel_messages\":{},\"channel_echoes\":{},",
                "\"bwe_kbps\":{},\"send_errors\":{},\"rtc_error\":{}}}"
            ),
            s(kind),
            t,
            self.offers,
            s(&self.ice),
            self.connected,
            s(&self.video_codec),
            self.video_frames,
            self.video_keyframes,
            self.video_bytes,
            self.video_gaps,
            self.video_echoed,
            self.video_packets_dropped,
            s(&self.audio_codec),
            self.audio_frames,
            self.audio_echoed,
            self.echo_errors,
            self.keyframe_requests_sent,
            self.keyframe_requests_failed,
            self.keyframe_requests_received,
            self.channel_open,
            s(&self.channel_label),
            self.channel_messages,
            self.channel_echoes,
            self.bwe_kbps,
            self.send_errors,
            s(&self.rtc_error),
        )
    }
}

/// One browser call.
struct Session {
    rtc: Rtc,
    local: SocketAddr,
    deadline: Instant,
    alive: bool,
    video: Option<Mid>,
    channel: Option<ChannelId>,
    /// Video is echoed only from a keyframe on: never before the first one, and not after a gap
    /// in the incoming frames until the next one (the browser's decoder would see broken refs).
    echo_video: bool,
    /// Throttles keyframe requests toward the browser's sender.
    last_keyframe_request: Option<Instant>,
    start_keyframe_pending: bool,
    /// `--drop-video`: every Nth outgoing packet with the video payload type is dropped, and every
    /// one with its RTX payload type (no repair).
    drop_video: u32,
    video_pt: Option<u8>,
    video_rtx_pt: Option<u8>,
    video_packets_out: u32,
    unreachable: HashSet<SocketAddr>,
}

impl Session {
    fn new(rtc: Rtc, local: SocketAddr, now: Instant, drop_video: u32) -> Session {
        Session {
            rtc,
            local,
            deadline: now,
            alive: true,
            video: None,
            channel: None,
            echo_video: false,
            last_keyframe_request: None,
            start_keyframe_pending: true,
            drop_video,
            video_pt: None,
            video_rtx_pt: None,
            video_packets_out: 0,
            unreachable: HashSet::new(),
        }
    }

    /// The str0m rule: after every mutation, poll until it hands back a timeout.
    fn drain(&mut self, socket: &UdpSocket, c: &mut Counters) {
        while self.alive {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(t)) => {
                    self.deadline = t;
                    return;
                }
                Ok(Output::Transmit(t)) => {
                    if self.drop(&t.contents) {
                        c.video_packets_dropped += 1;
                        continue;
                    }
                    if let Err(e) = socket.send_to(&t.contents, t.destination) {
                        // Expected while ICE probes pairs the OS can't route (a loopback socket
                        // toward the browser's LAN candidates): logged once per destination.
                        c.send_errors += 1;
                        if self.unreachable.insert(t.destination) {
                            eprintln!("send_to {}: {e}", t.destination);
                        }
                    }
                }
                Ok(Output::Event(e)) => self.on_event(e, c),
                Err(e) => {
                    eprintln!("poll_output: {e:?}");
                    c.rtc_error = format!("{e:?}");
                    self.rtc.disconnect();
                    self.alive = false;
                }
            }
        }
    }

    /// RTP (version 2, not RTCP): all RTX of the video, every `drop_video`th video packet.
    fn drop(&mut self, p: &[u8]) -> bool {
        if self.drop_video == 0 || p.len() < 12 || p[0] >> 6 != 2 || (192..=223).contains(&p[1]) {
            return false;
        }
        let pt = Some(p[1] & 0x7f);
        if pt == self.video_rtx_pt {
            return true;
        }
        if pt != self.video_pt {
            return false;
        }
        self.video_packets_out += 1;
        self.video_packets_out % self.drop_video == 0
    }

    fn on_event(&mut self, e: Event, c: &mut Counters) {
        match e {
            Event::IceConnectionStateChange(s) => {
                eprintln!("ice: {s:?}");
                c.ice = format!("{s:?}");
            }
            Event::Connected => {
                eprintln!("connected");
                c.connected = true;
                self.rtc.bwe().set_bitrate(Bitrate::kbps(300), Bitrate::mbps(2));
                self.start_keyframe(c);
            }
            Event::MediaAdded(m) => {
                // str0m emits it for the offered m-lines on the answering side after Connected.
                eprintln!("media added: {:?} {:?} {:?}", m.mid, m.kind, m.direction);
                if m.kind == MediaKind::Video {
                    self.video = Some(m.mid);
                    self.start_keyframe(c);
                }
            }
            Event::MediaData(d) => self.on_media(d, c),
            Event::KeyframeRequest(r) => {
                // The browser can't decode our echo of its own stream: only its encoder can fix
                // that, so the request goes back to it on the same m-line.
                c.keyframe_requests_received += 1;
                if Some(r.mid) == self.video {
                    self.request_keyframe(r.kind, "forwarded", c);
                }
            }
            Event::ChannelOpen(id, label) => {
                eprintln!("channel open: {id:?} {label:?}");
                self.channel = Some(id);
                c.channel_open = true;
                c.channel_label = label;
            }
            Event::ChannelData(d) => {
                c.channel_messages += 1;
                if let Some(mut ch) = self.rtc.channel(d.id) {
                    match ch.write(d.binary, &d.data) {
                        Ok(_) => c.channel_echoes += 1,
                        Err(e) => eprintln!("channel echo: {e:?}"),
                    }
                }
            }
            Event::ChannelClose(id) => eprintln!("channel close: {id:?}"),
            Event::EgressBitrateEstimate(BweKind::Twcc { estimate, .. }) => {
                c.bwe_kbps = estimate.as_u64() / 1000;
            }
            _ => {}
        }
    }

    /// Ask the browser for a keyframe right away, as a receiver joining a stream would: once both
    /// the connection and the video m-line are there (Connected comes first), retried on the first
    /// frames if the receive stream isn't known yet.
    fn start_keyframe(&mut self, c: &mut Counters) {
        if self.start_keyframe_pending && c.connected && self.video.is_some() {
            self.start_keyframe_pending = !self.request_keyframe(KeyframeRequestKind::Pli, "start", c);
        }
    }

    fn on_media(&mut self, d: MediaData, c: &mut Counters) {
        let spec = d.params.spec();
        let video = spec.codec == Codec::H264;
        if video {
            self.start_keyframe(c);
            if c.video_codec.is_empty() {
                c.video_codec = format!("{} pt={} {}", spec.codec, *d.pt, spec.format);
                self.video_pt = Some(*d.params.pt());
                self.video_rtx_pt = d.params.resend().map(|p| *p);
                eprintln!("video in: {}", c.video_codec);
            }
            c.video_frames += 1;
            c.video_bytes += d.data.len() as u64;
            if !d.contiguous {
                c.video_gaps += 1;
                self.echo_video = false;
            }
            if d.is_keyframe() {
                c.video_keyframes += 1;
                self.echo_video = true;
            } else if !self.echo_video {
                self.request_keyframe(KeyframeRequestKind::Pli, "waiting for a keyframe", c);
                return;
            }
        } else {
            if c.audio_codec.is_empty() {
                c.audio_codec = format!("{} pt={} {}", spec.codec, *d.pt, spec.format);
                eprintln!("audio in: {}", c.audio_codec);
            }
            c.audio_frames += 1;
        }

        // The echo: the same frame, back on the m-line it arrived on.
        let Some(w) = self.rtc.writer(d.mid) else {
            c.echo_errors += 1;
            return;
        };
        let Some(pt) = w.match_params(d.params) else {
            c.echo_errors += 1;
            return;
        };
        match w.write(pt, d.network_time, d.time, d.data) {
            Ok(()) if video => c.video_echoed += 1,
            Ok(()) => c.audio_echoed += 1,
            Err(e) => {
                c.echo_errors += 1;
                eprintln!("echo write: {e:?}");
            }
        }
    }

    /// True when the request went out (or one did less than 500 ms ago).
    fn request_keyframe(&mut self, kind: KeyframeRequestKind, why: &str, c: &mut Counters) -> bool {
        let now = Instant::now();
        if self.last_keyframe_request.is_some_and(|t| now - t < Duration::from_millis(500)) {
            return true;
        }
        let Some(mut w) = self.video.and_then(|mid| self.rtc.writer(mid)) else {
            c.keyframe_requests_failed += 1;
            eprintln!("keyframe request ({why}): no video writer");
            return false;
        };
        let kind = if w.is_request_keyframe_possible(kind) { kind } else { KeyframeRequestKind::Pli };
        match w.request_keyframe(None, kind) {
            Ok(()) => {
                c.keyframe_requests_sent += 1;
                self.last_keyframe_request = Some(now);
                eprintln!("keyframe request ({why}): {kind:?}");
                true
            }
            Err(e) => {
                c.keyframe_requests_failed += 1;
                eprintln!("keyframe request ({why}) failed: {e:?}");
                false
            }
        }
    }

    fn receive(&mut self, socket: &UdpSocket, source: SocketAddr, bytes: &[u8], c: &mut Counters) {
        let Ok(contents) = bytes.try_into() else { return };
        let now = Instant::now();
        let input = Input::Receive(now, Receive { proto: Protocol::Udp, source, destination: self.local, contents });
        if let Err(e) = self.rtc.handle_input(input) {
            eprintln!("handle_input(receive): {e:?}");
        }
        self.drain(socket, c);
    }

    fn timeout(&mut self, socket: &UdpSocket, c: &mut Counters) {
        if let Err(e) = self.rtc.handle_input(Input::Timeout(Instant::now())) {
            eprintln!("handle_input(timeout): {e:?}");
        }
        self.drain(socket, c);
    }
}

fn main() {
    if std::env::var_os("RUST_LOG").is_some() {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(io::stderr)
            .with_ansi(false)
            .init();
    }
    let a = args();
    let start = Instant::now();

    let socket = UdpSocket::bind(SocketAddr::new(a.host, a.udp_port)).unwrap_or_else(|e| die(&format!("udp bind: {e}")));
    let udp = socket.local_addr().unwrap();
    let listener =
        TcpListener::bind(SocketAddr::new(a.host, a.http_port)).unwrap_or_else(|e| die(&format!("http bind: {e}")));
    let http = listener.local_addr().unwrap();

    let (tx, rx) = mpsc::channel::<Rtc>();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let tx = tx.clone();
            thread::spawn(move || {
                if let Err(e) = serve(stream, &tx, udp) {
                    eprintln!("http: {e}");
                }
            });
        }
    });

    println!("{{\"type\":\"listening\",\"url\":\"http://{http}/\",\"udp\":\"{udp}\",\"seconds\":{}}}", a.seconds);
    flush();

    let mut c = Counters { ice: "none".into(), ..Default::default() };
    let mut session: Option<Session> = None;
    let mut buf = vec![0u8; 2000];
    let end = start + Duration::from_secs(a.seconds);
    let mut next_print = start + Duration::from_secs(1);

    loop {
        let now = Instant::now();
        if now >= end {
            break;
        }
        while let Ok(rtc) = rx.try_recv() {
            // A new offer replaces the call in progress.
            c = Counters { offers: c.offers + 1, ice: "new".into(), ..Default::default() };
            let mut s = Session::new(rtc, udp, now, a.drop_video);
            s.drain(&socket, &mut c);
            session = Some(s);
        }
        if now >= next_print {
            println!("{}", c.json("stats", (now - start).as_secs_f64()));
            flush();
            next_print += Duration::from_secs(1);
        }

        let mut wake = (now + Duration::from_millis(50)).min(next_print).min(end);
        if let Some(s) = session.as_ref().filter(|s| s.alive) {
            wake = wake.min(s.deadline);
        }
        // A zero read timeout is an error: at least 1 ms.
        let wait = wake.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
        socket.set_read_timeout(Some(wait)).expect("set_read_timeout");
        match socket.recv_from(&mut buf) {
            Ok((n, source)) => {
                if let Some(s) = session.as_mut().filter(|s| s.alive) {
                    s.receive(&socket, source, &buf[..n], &mut c);
                }
            }
            // WouldBlock on Windows, TimedOut elsewhere. ConnectionReset is Windows reporting an
            // ICMP port-unreachable for an earlier send_to: not an error for a UDP server.
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::ConnectionReset) => {}
            Err(e) => die(&format!("recv_from: {e}")),
        }
        if let Some(s) = session.as_mut().filter(|s| s.alive) {
            if Instant::now() >= s.deadline {
                s.timeout(&socket, &mut c);
            }
        }
    }

    println!("{}", c.json("final", (Instant::now() - start).as_secs_f64()));
    flush();
}

fn flush() {
    let _ = io::stdout().flush();
}

/// The signaling endpoint: GET / is the page, POST /offer takes an SDP offer and returns the answer.
fn serve(mut stream: TcpStream, tx: &mpsc::Sender<Rtc>, udp: SocketAddr) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").split('?').next().unwrap_or("/").to_string();
    let mut length = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                length = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;

    let (status, kind, content) = match (method.as_str(), path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => ("200 OK", "text/html; charset=utf-8", page().into_bytes()),
        ("POST", "/offer") => match answer(&body, tx, udp) {
            Ok(sdp) => ("200 OK", "application/sdp", sdp.into_bytes()),
            Err(e) => {
                eprintln!("offer rejected: {e}");
                ("400 Bad Request", "text/plain; charset=utf-8", e.into_bytes())
            }
        },
        ("GET", "/favicon.ico") => ("204 No Content", "text/plain", Vec::new()),
        _ => ("404 Not Found", "text/plain", b"not found".to_vec()),
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        content.len()
    )?;
    stream.write_all(&content)?;
    stream.flush()
}

/// The page is read from the source tree at request time (edits need no rebuild), else the copy
/// compiled in.
fn page() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/page/index.html"))
        .unwrap_or_else(|_| include_str!("../page/index.html").to_string())
}

fn answer(body: &[u8], tx: &mpsc::Sender<Rtc>, udp: SocketAddr) -> Result<String, String> {
    let sdp = std::str::from_utf8(body).map_err(|_| "offer is not UTF-8".to_string())?;
    let offer = SdpOffer::from_sdp_string(sdp).map_err(|e| format!("offer does not parse: {e:?}"))?;
    let mut rtc = config().build(Instant::now());
    let candidate = Candidate::host(udp, "udp").map_err(|e| format!("host candidate: {e:?}"))?;
    rtc.add_local_candidate(candidate);
    let answer = rtc.sdp_api().accept_offer(offer).map_err(|e| format!("accept_offer: {e:?}"))?;
    let text = answer.to_sdp_string();
    eprintln!("---- offer ----\n{}\n---- answer ----\n{}", sdp.trim_end(), text.trim_end());
    // The loop thread drains it first thing (accept_offer was a mutation).
    tx.send(rtc).map_err(|_| "peer loop is gone".to_string())?;
    Ok(text)
}
