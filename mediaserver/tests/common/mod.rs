//! What the server's tests share: an in-process server on loopback, phones (core's `Connection`),
//! the backend's HTTP calls, fake media, and a UDP proxy that plays a NAT (it can go silent, and
//! it can move to a new outward port).

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tinyrtc::connection::{AudioOut, Config as PhoneConfig, Connection, Event, State, VideoOut};
use tinyrtc_mediaserver::{Config, Server};

pub const SECRET: &str = "test-secret";
pub const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// A server on 127.0.0.1, ports chosen by the OS; `tune` sets the timeouts. Its log lines come
/// out of the receiver too.
pub fn server(tune: impl FnOnce(&mut Config)) -> (Server, Receiver<String>) {
    let (tx, log) = mpsc::channel();
    let mut config = Config::new(LOCALHOST, SECRET);
    config.udp = "127.0.0.1:0".parse().unwrap();
    config.http = "127.0.0.1:0".parse().unwrap();
    config.log = Some(tx);
    tune(&mut config);
    (Server::start(config).expect("server"), log)
}

/// One request of the backend: (status, body).
pub fn http(server: &Server, method: &str, path: &str, body: &str) -> (u16, String) {
    http_raw(server.http_addr(), method, path, Some(SECRET), body)
}

pub fn http_raw(addr: SocketAddr, method: &str, path: &str, secret: Option<&str>, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).expect("connect to the API");
    stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let auth = secret.map(|s| format!("Authorization: Bearer {s}\r\n")).unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: test\r\n{auth}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("a response");
    let status = response.split(' ').nth(1).and_then(|s| s.parse().ok()).expect("a status line");
    let body = response.split_once("\r\n\r\n").map(|(_, b)| b.to_owned()).unwrap_or_default();
    (status, body)
}

/// The value of a string field of a flat JSON object: `"key":"value"`.
pub fn field<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let at = json.find(&format!("\"{key}\":\""))? + key.len() + 4;
    json[at..].split('"').next()
}

/// One participant's object in GET /rooms/{id}.
pub fn participant<'a>(json: &'a str, id: &str) -> Option<&'a str> {
    let at = json.find(&format!("{{\"id\":\"{id}\""))?;
    let end = json[at..].find('}')? + at + 1;
    Some(&json[at..end])
}

pub fn create_room(server: &Server) -> String {
    let (status, body) = http(server, "POST", "/rooms", "");
    assert_eq!(status, 201, "{body}");
    field(&body, "id").expect("an id").to_owned()
}

/// The ICE ufrag of a description (what the backend reads to reserve a call).
pub fn ufrag_of(sdp: &str) -> String {
    sdp.lines().find_map(|l| l.strip_prefix("a=ice-ufrag:")).expect("a=ice-ufrag").trim().to_owned()
}

pub fn until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < limit, "timed out after {limit:?} waiting for: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// What one phone connection saw.
#[derive(Default, Debug)]
pub struct Seen {
    pub states: Vec<State>,
    pub frames: u32,
    pub keyframes: u32,
    pub rotations_wrong: u32,
    pub audio: u32,
    pub keyframe_requests: u32,
    pub opened: Vec<(u32, String)>,
    pub messages: Vec<Vec<u8>>,
}

pub struct Phone {
    pub name: String,
    pub conn: Connection,
    events: Receiver<Event>,
    pub seen: Seen,
    /// Sending: the next frame and packet, and how many keyframe requests were answered.
    video_n: u32,
    audio_n: u64,
    next_video: Option<Instant>,
    next_audio: Option<Instant>,
    answered: u32,
}

impl Phone {
    pub fn new(name: &str, relay_only: bool) -> Phone {
        let (tx, events) = mpsc::channel();
        let config = PhoneConfig { bind: Some(LOCALHOST), relay_only, ..Default::default() };
        let conn = Connection::new(config, Box::new(move |e| drop(tx.send(e)))).expect("connection");
        Phone {
            name: name.to_owned(),
            conn,
            events,
            seen: Seen::default(),
            video_n: 0,
            audio_n: 0,
            next_video: None,
            next_audio: None,
            answered: 0,
        }
    }

    pub fn pump(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            let seen = &mut self.seen;
            match event {
                Event::State(s) => seen.states.push(s),
                Event::VideoFrame(f) => {
                    seen.frames += 1;
                    seen.keyframes += f.keyframe as u32;
                    seen.rotations_wrong += (f.rotation != 90) as u32;
                }
                Event::AudioPacket(_) => seen.audio += 1,
                Event::KeyframeRequested => seen.keyframe_requests += 1,
                Event::ChannelOpen { id, label } => seen.opened.push((id, label)),
                Event::ChannelMessage { data, .. } => seen.messages.push(data),
                _ => {}
            }
        }
    }

    pub fn state(&self) -> Option<State> {
        self.seen.states.last().copied()
    }

    pub fn connected(&self) -> bool {
        self.state() == Some(State::Connected)
    }

    /// The media of a phone that publishes: video at 30 fps (a keyframe every second, and one
    /// for every request), Opus every 20 ms. Sends what is due now.
    pub fn send_due(&mut self) {
        let now = Instant::now();
        let video_due = *self.next_video.get_or_insert(now);
        if now >= video_due {
            let key = self.video_n.is_multiple_of(30) || self.seen.keyframe_requests > self.answered;
            if key {
                self.answered = self.seen.keyframe_requests;
            }
            self.conn.send_video(VideoOut { data: fake_h264(key, self.video_n), captured: now, rotation: 90 });
            self.video_n += 1;
            self.next_video = Some(video_due + Duration::from_millis(33));
        }
        let audio_due = *self.next_audio.get_or_insert(now);
        if now >= audio_due {
            let data: Arc<[u8]> = (0..40u8).map(|i| i ^ self.audio_n as u8).collect::<Vec<_>>().into();
            self.conn.send_audio(AudioOut { data, time: self.audio_n * 960, captured: now });
            self.audio_n += 1;
            self.next_audio = Some(audio_due + Duration::from_millis(20));
        }
    }

    pub fn sent_frames(&self) -> u32 {
        self.video_n
    }
    pub fn sent_audio(&self) -> u32 {
        self.audio_n as u32
    }
}

/// Runs the phones for `how_long`: the publishers send, everyone's events are taken.
pub fn run_for(how_long: Duration, publishers: &mut [&mut Phone], others: &mut [&mut Phone]) {
    let start = Instant::now();
    while start.elapsed() < how_long {
        for p in publishers.iter_mut() {
            p.pump();
            p.send_due();
        }
        for p in others.iter_mut() {
            p.pump();
        }
        std::thread::sleep(Duration::from_millis(3));
    }
    publishers.iter_mut().for_each(|p| p.pump());
    others.iter_mut().for_each(|p| p.pump());
}

/// An Annex B access unit with real NAL headers and filler below them (core's tests' shape).
pub fn fake_h264(key: bool, n: u32) -> Arc<[u8]> {
    let mut au = Vec::new();
    let mut nal = |header: u8, len: usize| {
        au.extend_from_slice(&[0, 0, 0, 1, header]);
        au.extend((0..len).map(|i| ((i as u32).wrapping_mul(31).wrapping_add(n) % 251) as u8 | 0x10));
    };
    if key {
        nal(0x67, 24);
        nal(0x68, 6);
        nal(0x65, 5_000);
    } else {
        nal(0x41, 600);
    }
    au.into()
}

/// A phone's offer, POSTed by the backend, the answer back into the phone. `via` = the phone
/// reaches the server through this proxy (the answer's candidate is rewritten to it).
pub fn join(server: &Server, phone: &Phone, path: &str, via: Option<&Proxy>) -> String {
    let offer = phone.conn.offer(false).expect("offer");
    let (status, answer) = http(server, "POST", path, &offer);
    assert_eq!(status, 200, "{path}: {answer}");
    assert!(answer.contains("a=ice-lite"), "the server answers ICE-lite");
    let candidates: Vec<&str> = answer.lines().filter(|l| l.starts_with("a=candidate")).collect();
    let public = server.public_addr();
    assert_eq!(candidates.len(), 1, "one candidate, the public address: {candidates:?}");
    assert!(candidates[0].contains(&format!(" {} {} typ host", public.ip(), public.port())), "{candidates:?}");
    let answer = match via {
        Some(proxy) => answer.replace(
            &format!(" {} {} typ host", public.ip(), public.port()),
            &format!(" {} {} typ host", proxy.addr.ip(), proxy.addr.port()),
        ),
        None => answer,
    };
    phone.conn.accept_answer(&answer).expect("accept the answer");
    answer
}

/// A NAT in front of a phone: what the phone sends to `addr` leaves from the proxy's outward
/// socket towards the server, and what comes back goes to the phone. `cut` = silence both ways;
/// `rebind` = a new outward port (the server sees the phone at a new address).
pub struct Proxy {
    pub addr: SocketAddr,
    /// Datagrams passed on: [towards the server, towards the phone].
    passed: Arc<[AtomicU64; 2]>,
    cut: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    outward: Arc<Mutex<Arc<UdpSocket>>>,
}

impl Proxy {
    pub fn new(server: SocketAddr) -> Proxy {
        let inward = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        inward.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        let addr = inward.local_addr().unwrap();
        let outward = Arc::new(Mutex::new(Arc::new(Self::socket())));
        let (cut, stop) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let phone: Arc<Mutex<Option<SocketAddr>>> = Arc::default();
        let passed: Arc<[AtomicU64; 2]> = Arc::default();
        {
            let (inward, outward, cut, stop, phone, passed) =
                (inward.clone(), outward.clone(), cut.clone(), stop.clone(), phone.clone(), passed.clone());
            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                while !stop.load(Ordering::Relaxed) {
                    let Ok((n, from)) = inward.recv_from(&mut buf) else { continue };
                    *phone.lock().unwrap() = Some(from);
                    if !cut.load(Ordering::Relaxed) {
                        let socket = outward.lock().unwrap().clone();
                        let _ = socket.send_to(&buf[..n], server);
                        passed[0].fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
        {
            let (outward, cut, stop, passed) = (outward.clone(), cut.clone(), stop.clone(), passed.clone());
            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                while !stop.load(Ordering::Relaxed) {
                    let socket = outward.lock().unwrap().clone();
                    let Ok((n, _)) = socket.recv_from(&mut buf) else { continue };
                    let to = *phone.lock().unwrap();
                    if let (false, Some(to)) = (cut.load(Ordering::Relaxed), to) {
                        let _ = inward.send_to(&buf[..n], to);
                        passed[1].fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
        Proxy { addr, passed, cut, stop, outward }
    }

    fn socket() -> UdpSocket {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        socket
    }

    pub fn passed(&self) -> [u64; 2] {
        [self.passed[0].load(Ordering::Relaxed), self.passed[1].load(Ordering::Relaxed)]
    }

    pub fn cut(&self, silent: bool) {
        self.cut.store(silent, Ordering::Relaxed);
    }

    /// Where the server sees the phone from.
    pub fn outward_addr(&self) -> SocketAddr {
        self.outward.lock().unwrap().local_addr().unwrap()
    }

    pub fn rebind(&self) -> SocketAddr {
        let socket = Arc::new(Self::socket());
        let addr = socket.local_addr().unwrap();
        *self.outward.lock().unwrap() = socket;
        addr
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The log lines so far.
pub fn lines(log: &Receiver<String>, into: &mut Vec<String>) {
    into.extend(log.try_iter());
}
