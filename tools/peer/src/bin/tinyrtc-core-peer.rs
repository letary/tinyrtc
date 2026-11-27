//! Interop probe on the library itself: the same HTTP shape and the same JSON counters as
//! src/main.rs (the raw str0m peer), but the peer is a `tinyrtc::connection::Connection` — the
//! negotiation, the SDP, the media and the channels of the core, nothing of str0m's own.
//!
//! Both directions of the offer:
//!   POST /offer  the browser offers (SDP text); the core's answer comes back
//!   GET  /offer  the core offers (a new connection); POST /answer takes the browser's answer
//!   POST /reoffer  the browser's ICE restart offer to the call in progress; the answer comes back
//!   GET  /restart  the core's ICE restart offer (`offer(true)`); POST /answer takes the answer
//!
//! What arrives is echoed back through the core's API: a video frame by `send_video` (from a
//! keyframe on), an Opus packet by `send_audio`, a channel message on its channel. A keyframe the
//! browser asks for is asked of the browser (only its encoder can make one).
//!
//! --relay: the browser is handed the core's address only as the media server's relay candidate
//! (`typ relay`, priority 255: mediaserver/src/call.rs): the descriptions it gets carry no candidate,
//! and `GET /candidate` serves that line for its `addIceCandidate` — Chrome must take a remote
//! relay-typed candidate and connect through it (the core is a plain connection: it takes
//! Chrome's candidates from Chrome's description).
//!
//! stdout carries only JSON lines (`listening`, `stats` every second, `final`); diagnostics and
//! both descriptions go to stderr.
//!
//!   tinyrtc-core-peer [--host 127.0.0.1] [--http-port 0] [--seconds 30] [--relay]

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tinyrtc::connection::{AudioOut, Config, Connection, Event, State, VideoOut};

struct Args {
    host: IpAddr,
    http_port: u16,
    seconds: u64,
    relay: bool,
}

fn args() -> Args {
    let mut a = Args { host: "127.0.0.1".parse().unwrap(), http_port: 0, seconds: 30, relay: false };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        if k == "--relay" {
            a.relay = true;
            continue;
        }
        let v = it.next().unwrap_or_else(|| die(&format!("{k} needs a value")));
        match k.as_str() {
            "--host" => a.host = v.parse().unwrap_or_else(|_| die("--host: not an IP address")),
            "--http-port" => a.http_port = v.parse().unwrap_or_else(|_| die("--http-port: not a port")),
            "--seconds" => a.seconds = v.parse().unwrap_or_else(|_| die("--seconds: not a number")),
            _ => die(&format!("unknown argument {k}")),
        }
    }
    a
}

fn die(msg: &str) -> ! {
    eprintln!("tinyrtc-core-peer: {msg}");
    std::process::exit(2);
}

/// The raw peer's counters, the same JSON (interop.mjs reads either).
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
    /// Core only: which side offered ("browser" / "core").
    offerer: String,
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
                "\"bwe_kbps\":{},\"send_errors\":{},\"rtc_error\":{},\"offerer\":{}}}"
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
            s(&self.offerer),
        )
    }
}

/// The call in progress: a new offer (either way) replaces it.
struct Call {
    generation: u64,
    conn: Arc<Connection>,
    offerer: &'static str,
}

struct Shared {
    host: IpAddr,
    events: mpsc::Sender<(u64, Event)>,
    call: Mutex<Option<Call>>,
    next_generation: Mutex<u64>,
    /// --relay: the core's address as the media server's relay line, served at GET /candidate.
    relay: Option<Mutex<Option<String>>>,
}

impl Shared {
    /// What the browser is handed of a description of ours. --relay: no candidate in it; its
    /// address becomes the relay line of GET /candidate.
    fn shown(&self, sdp: String) -> String {
        let Some(relay) = &self.relay else { return sdp };
        let mut out = Vec::new();
        for line in sdp.split("\r\n") {
            let Some(candidate) = line.strip_prefix("a=candidate:") else {
                out.push(line);
                continue;
            };
            // foundation component transport priority address port typ host
            let words: Vec<&str> = candidate.split_whitespace().collect();
            if let [_, _, _, _, address, port, ..] = words[..] {
                *relay.lock().unwrap() = Some(format!("candidate:1 1 udp 255 {address} {port} typ relay raddr 0.0.0.0 rport 0"));
            }
        }
        out.join("\r\n")
    }

    fn connect(&self) -> Result<(u64, Arc<Connection>), String> {
        let generation = {
            let mut next = self.next_generation.lock().unwrap();
            *next += 1;
            *next
        };
        let events = self.events.clone();
        let config = Config { bind: Some(self.host), ..Default::default() };
        let conn = Connection::new(config, Box::new(move |e| drop(events.send((generation, e)))))
            .map_err(|e| format!("connection: {e}"))?;
        Ok((generation, Arc::new(conn)))
    }

    fn install(&self, generation: u64, conn: Arc<Connection>, offerer: &'static str) {
        // The call it replaces is dropped here, outside the lock's critical work: the drop says
        // goodbye and joins the connection's threads.
        let before = self.call.lock().unwrap().replace(Call { generation, conn, offerer });
        drop(before);
    }
}

fn main() {
    let a = args();
    let start = Instant::now();
    let listener =
        TcpListener::bind(SocketAddr::new(a.host, a.http_port)).unwrap_or_else(|e| die(&format!("http bind: {e}")));
    let http = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel::<(u64, Event)>();
    let relay = a.relay.then(|| Mutex::new(None));
    let shared = Arc::new(Shared { host: a.host, events: tx, call: Mutex::new(None), next_generation: Mutex::new(0), relay });

    {
        let shared = shared.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let shared = shared.clone();
                thread::spawn(move || {
                    if let Err(e) = serve(stream, &shared) {
                        eprintln!("http: {e}");
                    }
                });
            }
        });
    }

    println!("{{\"type\":\"listening\",\"url\":\"http://{http}/\",\"udp\":\"{}:0\",\"seconds\":{}}}", a.host, a.seconds);
    flush();

    let mut c = Counters { ice: "none".into(), ..Default::default() };
    let mut current = 0u64;
    // Video is echoed from a keyframe on (the core already withholds what follows a loss).
    let mut echo_video = false;
    let end = start + Duration::from_secs(a.seconds);
    let mut next_print = start + Duration::from_secs(1);
    loop {
        let now = Instant::now();
        if now >= end {
            break;
        }
        if now >= next_print {
            println!("{}", c.json("stats", (now - start).as_secs_f64()));
            flush();
            next_print += Duration::from_secs(1);
        }
        let wait = next_print.min(end).saturating_duration_since(Instant::now());
        let (generation, event) = match rx.recv_timeout(wait) {
            Ok(got) => got,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let call = {
            let call = shared.call.lock().unwrap();
            call.as_ref().map(|call| (call.generation, call.conn.clone(), call.offerer))
        };
        let Some((live, conn, offerer)) = call else { continue };
        if generation != live {
            continue;
        }
        if current != live {
            // A new call: its own numbers.
            current = live;
            c = Counters { offers: c.offers + 1, ice: "new".into(), offerer: offerer.into(), ..Default::default() };
            echo_video = false;
        }
        match event {
            Event::State(state) => {
                eprintln!("state: {state:?}");
                c.ice = format!("{state:?}");
                if state == State::Connected && !c.connected {
                    c.connected = true;
                    // A receiver joining a stream asks for a picture to start from.
                    conn.request_keyframe();
                    c.keyframe_requests_sent += 1;
                }
            }
            Event::VideoFrame(frame) => {
                if c.video_codec.is_empty() {
                    c.video_codec = "H264 (the core's profile)".into();
                }
                c.video_frames += 1;
                c.video_bytes += frame.data.len() as u64;
                if frame.keyframe {
                    c.video_keyframes += 1;
                    echo_video = true;
                }
                if echo_video {
                    conn.send_video(VideoOut { data: frame.data, captured: frame.received, rotation: frame.rotation });
                    c.video_echoed += 1;
                }
            }
            Event::AudioPacket(packet) => {
                if c.audio_codec.is_empty() {
                    c.audio_codec = "opus (the core's profile)".into();
                }
                c.audio_frames += 1;
                conn.send_audio(AudioOut { data: packet.data, time: packet.time, captured: packet.received });
                c.audio_echoed += 1;
            }
            Event::KeyframeRequested => {
                // The browser cannot decode our echo of its own stream: only its encoder can fix
                // that (the core throttles the requests it sends).
                c.keyframe_requests_received += 1;
                conn.request_keyframe();
                c.keyframe_requests_sent += 1;
            }
            Event::ChannelOpen { id, label } => {
                eprintln!("channel open: {id} {label:?}");
                c.channel_open = true;
                c.channel_label = label;
            }
            Event::ChannelMessage { id, data, binary } => {
                c.channel_messages += 1;
                match conn.channel_send(id, data, binary) {
                    Ok(()) => c.channel_echoes += 1,
                    Err(e) => {
                        c.echo_errors += 1;
                        eprintln!("channel echo: {e:?}");
                    }
                }
            }
            Event::ChannelClosed { id } => eprintln!("channel closed: {id}"),
            Event::SendEstimate(bps) => c.bwe_kbps = bps / 1000,
            Event::LocalCandidate(line) => eprintln!("local candidate (not signaled): {line}"),
            Event::RemoteMedia { media, active } => eprintln!("remote media: {media:?} {active}"),
            Event::IceRestartNeeded => eprintln!("ice restart needed"),
            Event::ChannelBufferedLow { .. } => {}
        }
    }

    println!("{}", c.json("final", (Instant::now() - start).as_secs_f64()));
    flush();
    // Say goodbye to the browser before exiting.
    let call = shared.call.lock().unwrap().take();
    drop(call);
}

fn flush() {
    let _ = io::stdout().flush();
}

fn serve(mut stream: TcpStream, shared: &Shared) -> io::Result<()> {
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

    let sdp = |result: Result<String, String>| match result {
        Ok(sdp) => ("200 OK", "application/sdp", sdp.into_bytes()),
        Err(e) => {
            eprintln!("refused: {e}");
            ("400 Bad Request", "text/plain; charset=utf-8", e.into_bytes())
        }
    };
    let (status, kind, content) = match (method.as_str(), path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => ("200 OK", "text/html; charset=utf-8", page().into_bytes()),
        ("POST", "/offer") => sdp(answer(&body, shared)),
        ("GET", "/offer") => sdp(offer(shared)),
        ("POST", "/answer") => sdp(accept_answer(&body, shared).map(|()| String::new())),
        // An ICE restart of the call in progress: the browser's new offer, or one of ours.
        ("POST", "/reoffer") => sdp(reoffer(&body, shared)),
        ("GET", "/restart") => sdp(restart(shared)),
        // --relay: the core's address as the media server's relay line.
        ("GET", "/candidate") => match shared.relay.as_ref().and_then(|relay| relay.lock().unwrap().clone()) {
            Some(line) => ("200 OK", "text/plain; charset=utf-8", line.into_bytes()),
            None => ("404 Not Found", "text/plain", b"no relay line".to_vec()),
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

/// The page of the raw peer, the same file.
fn page() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/page/index.html"))
        .unwrap_or_else(|_| include_str!("../../page/index.html").to_string())
}

/// The browser offers: a new connection answers.
fn answer(body: &[u8], shared: &Shared) -> Result<String, String> {
    let sdp = std::str::from_utf8(body).map_err(|_| "offer is not UTF-8".to_string())?;
    let (generation, conn) = shared.connect()?;
    let answer = conn.accept_offer(sdp).map_err(|e| format!("accept_offer: {e:?}"))?;
    eprintln!("---- offer (browser) ----\n{}\n---- answer (core) ----\n{}", sdp.trim_end(), answer.trim_end());
    shared.install(generation, conn, "browser");
    Ok(shared.shown(answer))
}

/// The core offers: a new connection, its offer; the answer comes with POST /answer.
fn offer(shared: &Shared) -> Result<String, String> {
    let (generation, conn) = shared.connect()?;
    let offer = conn.offer(false).map_err(|e| format!("offer: {e:?}"))?;
    eprintln!("---- offer (core) ----\n{}", offer.trim_end());
    shared.install(generation, conn, "core");
    Ok(shared.shown(offer))
}

fn current(shared: &Shared) -> Result<Arc<Connection>, String> {
    shared.call.lock().unwrap().as_ref().map(|call| call.conn.clone()).ok_or_else(|| "no call in progress".to_string())
}

/// The browser restarts ICE: its new offer to the connection in progress.
fn reoffer(body: &[u8], shared: &Shared) -> Result<String, String> {
    let sdp = std::str::from_utf8(body).map_err(|_| "offer is not UTF-8".to_string())?;
    let answer = current(shared)?.accept_offer(sdp).map_err(|e| format!("accept_offer (restart): {e:?}"))?;
    eprintln!("---- restart offer (browser) ----\n{}\n---- restart answer (core) ----\n{}", sdp.trim_end(), answer.trim_end());
    Ok(shared.shown(answer))
}

/// The core restarts ICE: its new offer; the answer comes with POST /answer.
fn restart(shared: &Shared) -> Result<String, String> {
    let offer = current(shared)?.offer(true).map_err(|e| format!("offer (restart): {e:?}"))?;
    eprintln!("---- restart offer (core) ----\n{}", offer.trim_end());
    Ok(shared.shown(offer))
}

fn accept_answer(body: &[u8], shared: &Shared) -> Result<(), String> {
    let sdp = std::str::from_utf8(body).map_err(|_| "answer is not UTF-8".to_string())?;
    eprintln!("---- answer (browser) ----\n{}", sdp.trim_end());
    let conn = shared.call.lock().unwrap().as_ref().map(|call| call.conn.clone()).ok_or("no offer was made")?;
    conn.accept_answer(sdp).map_err(|e| format!("accept_answer: {e:?}"))
}
