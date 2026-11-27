//! The core's own SDP against the other peers it must work with, on real UDP sockets of the
//! loopback interface, in real time: another connection of the library (ICE restarts either way,
//! offers crossing, descriptions without a=ssrc lines), str0m's SDP API (as the offerer and as the
//! answerer, full ICE; the media server in mediaserver/ is the ICE-lite case), and descriptions mutated
//! at random, which may be refused but must never kill a connection.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use str0m::change::{SdpAnswer, SdpOffer};
use str0m::format::Codec;
use str0m::media::{MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event as RtcEvent, Input, Output, Rtc};
use tinyrtc::connection::{ChannelOptions, Config, Connection, Error, Event, State, VideoOut};

fn peer() -> (Connection, Receiver<Event>) {
    let (tx, rx) = mpsc::channel();
    let config = Config { bind: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)), ..Default::default() };
    (Connection::new(config, Box::new(move |e| drop(tx.send(e)))).expect("connection"), rx)
}

fn fake_h264(key: bool, n: u32) -> Arc<[u8]> {
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

/// What one connection saw.
#[derive(Default, Debug)]
struct Seen {
    states: Vec<State>,
    frames: u32,
    opened: Vec<(u32, String)>,
    messages: u32,
}

impl Seen {
    fn take(&mut self, events: &Receiver<Event>) {
        for event in events.try_iter() {
            match event {
                Event::State(s) => self.states.push(s),
                Event::VideoFrame(_) => self.frames += 1,
                Event::ChannelOpen { id, label } => self.opened.push((id, label)),
                Event::ChannelMessage { .. } => self.messages += 1,
                _ => {}
            }
        }
    }
    fn connected(&self) -> bool {
        self.states.last() == Some(&State::Connected)
    }
}

fn until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < limit, "timed out waiting for: {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

/// `seconds` of video both ways at 30 fps; returns the frames each side got.
fn video_both_ways(a: &Connection, b: &Connection, sa: &mut Seen, sb: &mut Seen, ea: &Receiver<Event>, eb: &Receiver<Event>, seconds: f64) -> (u32, u32, u32) {
    let (fa, fb) = (sa.frames, sb.frames);
    let start = Instant::now();
    let mut n = 0u32;
    while start.elapsed().as_secs_f64() < seconds {
        for c in [a, b] {
            c.send_video(VideoOut { data: fake_h264(n.is_multiple_of(30), n), captured: Instant::now(), rotation: 0 });
        }
        n += 1;
        thread::sleep(Duration::from_millis(33));
        sa.take(ea);
        sb.take(eb);
    }
    thread::sleep(Duration::from_millis(300));
    sa.take(ea);
    sb.take(eb);
    (sa.frames - fa, sb.frames - fb, n)
}

fn ufrag(sdp: &str) -> &str {
    sdp.lines().find_map(|l| l.strip_prefix("a=ice-ufrag:")).expect("a=ice-ufrag").trim()
}

#[test]
fn an_ice_restart_either_way_keeps_the_call() {
    let (a, ea) = peer();
    let (b, eb) = peer();
    let (mut sa, mut sb) = (Seen::default(), Seen::default());
    let offer = a.offer(false).unwrap();
    let answer = b.accept_offer(&offer).unwrap();
    a.accept_answer(&answer).unwrap();
    until("connected", Duration::from_secs(10), || {
        sa.take(&ea);
        sb.take(&eb);
        sa.connected() && sb.connected()
    });
    // Nothing new to offer without a restart.
    assert_eq!(a.offer(false), Err(Error::State));

    // The offering side restarts: new credentials both ways, the same sections and numbers.
    let restart = a.offer(true).unwrap();
    assert_ne!(ufrag(&restart), ufrag(&offer));
    let reanswer = b.accept_offer(&restart).unwrap();
    assert_ne!(ufrag(&reanswer), ufrag(&answer));
    for line in ["m=audio 9 UDP/TLS/RTP/SAVPF 111", "m=video 9 UDP/TLS/RTP/SAVPF 108 109", "a=group:BUNDLE 0 1 2", "a=setup:actpass"] {
        assert!(restart.contains(line), "{line}\n{restart}");
    }
    a.accept_answer(&reanswer).unwrap();
    let (at_a, at_b, n) = video_both_ways(&a, &b, &mut sa, &mut sb, &ea, &eb, 1.5);
    assert!(at_a + 3 >= n && at_b + 3 >= n, "video after the restart: {at_a} / {at_b} of {n}");

    // The answering side restarts: it offers now, with the session as agreed.
    let restart = b.offer(true).unwrap();
    assert!(restart.contains("a=setup:actpass") && restart.contains("a=group:BUNDLE 0 1 2"), "{restart}");
    let reanswer = a.accept_offer(&restart).unwrap();
    // a stays the DTLS server it became (str0m's answer to actpass): passive again.
    assert!(reanswer.contains("a=setup:passive") || reanswer.contains("a=setup:active"), "{reanswer}");
    b.accept_answer(&reanswer).unwrap();
    let (at_a, at_b, n) = video_both_ways(&a, &b, &mut sa, &mut sb, &ea, &eb, 1.5);
    assert!(at_a + 3 >= n && at_b + 3 >= n, "video after the second restart: {at_a} / {at_b} of {n}");
    assert!(sa.connected() && sb.connected(), "{:?} / {:?}", sa.states, sb.states);
}

#[test]
fn without_ssrc_lines_the_streams_are_found_by_their_mid() {
    let strip = |sdp: &str| -> String {
        sdp.split_inclusive('\n').filter(|l| !l.starts_with("a=ssrc")).collect()
    };
    let (a, ea) = peer();
    let (b, eb) = peer();
    let (mut sa, mut sb) = (Seen::default(), Seen::default());
    let offer = strip(&a.offer(false).unwrap());
    assert!(!offer.contains("a=ssrc"));
    let answer = strip(&b.accept_offer(&offer).unwrap());
    a.accept_answer(&answer).unwrap();
    until("connected", Duration::from_secs(10), || {
        sa.take(&ea);
        sb.take(&eb);
        sa.connected() && sb.connected()
    });
    let (at_a, at_b, n) = video_both_ways(&a, &b, &mut sa, &mut sb, &ea, &eb, 1.5);
    assert!(at_a + 3 >= n && at_b + 3 >= n, "video by mid alone: {at_a} / {at_b} of {n}");
}

#[test]
fn offers_that_cross_end_in_one_call() {
    // Both offer; a takes b's offer (giving up its own), answers; b takes the answer.
    let (a, ea) = peer();
    let (b, eb) = peer();
    let (mut sa, mut sb) = (Seen::default(), Seen::default());
    let early = a.channel_open("early", ChannelOptions::default()).unwrap();
    let _ours = a.offer(false).unwrap();
    let theirs = b.offer(false).unwrap();
    let answer = a.accept_offer(&theirs).unwrap();
    b.accept_answer(&answer).unwrap();
    assert_eq!(a.accept_answer(&answer), Err(Error::State), "a's own offer is no longer pending");
    until("connected and the early channel open", Duration::from_secs(10), || {
        sa.take(&ea);
        sb.take(&eb);
        sa.connected() && sb.connected() && sb.opened.iter().any(|(_, l)| l == "early")
    });
    assert!(a.channel_send(early, b"x".to_vec(), true).is_ok());
    let (at_a, at_b, n) = video_both_ways(&a, &b, &mut sa, &mut sb, &ea, &eb, 1.0);
    assert!(at_a + 3 >= n && at_b + 3 >= n, "{at_a} / {at_b} of {n}");
}

#[test]
fn a_channel_asked_for_between_offer_and_answer_opens() {
    let (a, ea) = peer();
    let (b, eb) = peer();
    let (mut sa, mut sb) = (Seen::default(), Seen::default());
    let offer = a.offer(false).unwrap();
    let between = a.channel_open("between", ChannelOptions::default()).unwrap();
    assert_eq!(a.channel_buffered(between), Ok(0));
    assert_eq!(a.channel_send(between, vec![1], true), Err(Error::State), "not open yet");
    let closed = a.channel_open("closed-before-open", ChannelOptions::default()).unwrap();
    a.channel_close(closed);
    let answer = b.accept_offer(&offer).unwrap();
    a.accept_answer(&answer).unwrap();
    until("the channel open on both sides", Duration::from_secs(10), || {
        sa.take(&ea);
        sb.take(&eb);
        sa.opened.iter().any(|(id, _)| *id == between) && sb.opened.iter().any(|(_, l)| l == "between")
    });
    thread::sleep(Duration::from_millis(200));
    sb.take(&eb);
    assert!(!sb.opened.iter().any(|(_, l)| l == "closed-before-open"), "{:?}", sb.opened);
}

#[test]
fn a_dead_connection_says_closed() {
    let (a, ea) = peer();
    let (b, eb) = peer();
    let (mut sa, mut sb) = (Seen::default(), Seen::default());
    let offer = a.offer(false).unwrap();
    a.accept_answer(&b.accept_offer(&offer).unwrap()).unwrap();
    until("connected", Duration::from_secs(10), || {
        sa.take(&ea);
        sb.take(&eb);
        sa.connected() && sb.connected()
    });
    drop(a);
    until("b closed", Duration::from_secs(5), || {
        sb.take(&eb);
        sb.states.last() == Some(&State::Closed)
    });
    assert_eq!(b.offer(true), Err(Error::Closed));
    assert_eq!(b.channel_open("x", ChannelOptions::default()), Err(Error::Closed));
}

/// `Connected` is ICE and DTLS; the SCTP handshake starts after it, and str0m refuses to close in
/// the middle of one. A side dropped at that moment must still say goodbye.
#[test]
fn a_connection_dropped_the_moment_it_connects_says_closed() {
    let wait_connected = |events: &Receiver<Event>| loop {
        match events.recv_timeout(Duration::from_secs(10)).expect("connected") {
            Event::State(State::Connected) => break,
            _ => {}
        }
    };
    for round in 0..12 {
        let (a, ea) = peer();
        let (b, eb) = peer();
        let offer = a.offer(false).unwrap();
        a.accept_answer(&b.accept_offer(&offer).unwrap()).unwrap();
        // Either side: only one of them starts the SCTP handshake.
        let (dropped, events, left, left_events) = if round % 2 == 0 { (a, ea, b, eb) } else { (b, eb, a, ea) };
        wait_connected(&events);
        let at = Instant::now();
        drop(dropped);
        assert!(at.elapsed() < Duration::from_secs(3), "round {round}: the drop took {:?}", at.elapsed());
        let mut seen = Seen::default();
        until("the other side closed", Duration::from_secs(3), || {
            seen.take(&left_events);
            seen.states.last() == Some(&State::Closed)
        });
        assert_eq!(left.offer(true), Err(Error::Closed));
    }
}

// ---------------------------------------------------------------------------------------------
// str0m's SDP API as the other side: a raw `Rtc` of the same profile on its own socket and thread.

type Job = Box<dyn FnOnce(&mut Raw) + Send>;

#[derive(Default, Debug, Clone)]
struct RawSeen {
    connected: bool,
    video_frames: u32,
    audio_frames: u32,
    channel_messages: u32,
}

struct Raw {
    rtc: Rtc,
    socket: UdpSocket,
    addr: SocketAddr,
    seen: Arc<Mutex<RawSeen>>,
    video: Option<Mid>,
    channel: Option<str0m::channel::ChannelId>,
    pending: Option<str0m::change::SdpPendingOffer>,
}

struct RawPeer {
    jobs: Sender<Job>,
    seen: Arc<Mutex<RawSeen>>,
    thread: Option<JoinHandle<()>>,
}

impl RawPeer {
    fn new() -> RawPeer {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        socket.set_read_timeout(Some(Duration::from_millis(2))).unwrap();
        let mut rtc = tinyrtc::profile::config().build(Instant::now());
        rtc.add_local_candidate(Candidate::host(addr, "udp").unwrap());
        let seen = Arc::new(Mutex::new(RawSeen::default()));
        let (jobs, rx) = mpsc::channel::<Job>();
        let mut raw = Raw { rtc, socket, addr, seen: seen.clone(), video: None, channel: None, pending: None };
        let thread = thread::spawn(move || raw.run(rx));
        RawPeer { jobs, seen, thread: Some(thread) }
    }

    fn with<T: Send + 'static>(&self, f: impl FnOnce(&mut Raw) -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::sync_channel(1);
        self.jobs.send(Box::new(move |raw: &mut Raw| drop(tx.send(f(raw))))).unwrap();
        rx.recv().unwrap()
    }

    fn seen(&self) -> RawSeen {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for RawPeer {
    fn drop(&mut self) {
        let _ = self.jobs.send(Box::new(|raw: &mut Raw| raw.rtc.disconnect()));
        drop(std::mem::replace(&mut self.jobs, mpsc::channel().0));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Raw {
    fn run(&mut self, jobs: Receiver<Job>) {
        let mut buf = vec![0u8; 2000];
        let mut next_frame = Instant::now();
        let mut n = 0u32;
        loop {
            loop {
                match jobs.try_recv() {
                    Ok(job) => job(self),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return,
                }
            }
            if !self.rtc.is_alive() {
                return;
            }
            let timeout = self.drain();
            if let Ok((len, source)) = self.socket.recv_from(&mut buf) {
                if let Ok(contents) = buf[..len].try_into() {
                    let input = Input::Receive(Instant::now(), Receive { proto: Protocol::Udp, source, destination: self.addr, contents });
                    let _ = self.rtc.handle_input(input);
                }
            }
            let now = Instant::now();
            if now >= timeout {
                let _ = self.rtc.handle_input(Input::Timeout(now));
            }
            // Its own video once connected: 30 fps.
            let connected = self.seen.lock().unwrap().connected;
            if let (true, Some(mid), true) = (connected, self.video, now >= next_frame) {
                next_frame = now + Duration::from_millis(33);
                if let Some(writer) = self.rtc.writer(mid) {
                    let pt = writer.payload_params().find(|p| p.spec().codec == Codec::H264).map(|p| p.pt());
                    if let Some(pt) = pt {
                        let time = MediaTime::from_90khz(n as u64 * 3000);
                        let _ = writer.write(pt, now, time, fake_h264(n.is_multiple_of(30), n));
                        n += 1;
                    }
                }
                if let Some(mut channel) = self.channel.and_then(|c| self.rtc.channel(c)) {
                    let _ = channel.write(false, b"from str0m");
                }
            }
        }
    }

    fn drain(&mut self) -> Instant {
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(at)) => return at,
                Ok(Output::Transmit(t)) => {
                    let _ = self.socket.send_to(&t.contents, t.destination);
                }
                Ok(Output::Event(event)) => {
                    let mut seen = self.seen.lock().unwrap();
                    match event {
                        RtcEvent::Connected => seen.connected = true,
                        RtcEvent::MediaAdded(m) if m.kind == MediaKind::Video => self.video = Some(m.mid),
                        RtcEvent::MediaData(d) if d.params.spec().codec == Codec::H264 => seen.video_frames += 1,
                        RtcEvent::MediaData(_) => seen.audio_frames += 1,
                        // The library's own control channel is not for messages of ours.
                        RtcEvent::ChannelOpen(id, label) if label != "tinyrtc" => self.channel = self.channel.or(Some(id)),
                        RtcEvent::ChannelData(_) => seen.channel_messages += 1,
                        _ => {}
                    }
                }
                Err(_) => return Instant::now() + Duration::from_millis(5),
            }
        }
    }
}

fn str0m_call(core_offers: bool) {
    let raw = RawPeer::new();
    let (core, events) = peer();
    let mut seen = Seen::default();
    let chat = core.channel_open("chat", ChannelOptions::default()).unwrap();
    if core_offers {
        let offer = core.offer(false).unwrap();
        let answer = raw.with(move |raw| {
            let offer = SdpOffer::from_sdp_string(&offer).expect("str0m parses our offer");
            raw.rtc.sdp_api().accept_offer(offer).expect("str0m takes our offer").to_sdp_string()
        });
        core.accept_answer(&answer).expect("we take str0m's answer");
    } else {
        let offer = raw.with(|raw| {
            let mut api = raw.rtc.sdp_api();
            api.add_media(MediaKind::Audio, str0m::media::Direction::SendRecv, None, None, None);
            raw.video = Some(api.add_media(MediaKind::Video, str0m::media::Direction::SendRecv, None, None, None));
            api.add_channel("from str0m".into());
            let (offer, pending) = api.apply().unwrap();
            raw.pending = Some(pending);
            offer.to_sdp_string()
        });
        let answer = core.accept_offer(&offer).expect("we take str0m's offer");
        raw.with(move |raw| {
            let answer = SdpAnswer::from_sdp_string(&answer).expect("str0m parses our answer");
            let pending = raw.pending.take().unwrap();
            raw.rtc.sdp_api().accept_answer(pending, answer).expect("str0m takes our answer");
        });
    }
    until("both connected", Duration::from_secs(10), || {
        seen.take(&events);
        seen.connected() && raw.seen().connected
    });
    let start = Instant::now();
    let mut n = 0u32;
    while start.elapsed() < Duration::from_secs(2) {
        core.send_video(VideoOut { data: fake_h264(n.is_multiple_of(30), n), captured: Instant::now(), rotation: 0 });
        let _ = core.channel_send(chat, b"from the core".to_vec(), false);
        n += 1;
        thread::sleep(Duration::from_millis(33));
        seen.take(&events);
    }
    thread::sleep(Duration::from_millis(300));
    seen.take(&events);
    let theirs = raw.seen();
    let who = if core_offers { "the core offers" } else { "str0m offers" };
    eprintln!("{who}: core got {} frames, {} messages; str0m got {theirs:?} of {n}", seen.frames, seen.messages);
    assert!(theirs.video_frames + 3 >= n, "{who}: str0m got {} of {n} frames", theirs.video_frames);
    assert!(seen.frames >= 40, "{who}: the core got {} frames from str0m", seen.frames);
    assert!(theirs.channel_messages > 0 && seen.messages > 0, "{who}: data both ways {theirs:?} {seen:?}");

    if core_offers {
        // str0m's SDP API takes our restart offer and answers it with new credentials.
        let restart = core.offer(true).unwrap();
        let answer = raw.with(move |raw| raw.rtc.sdp_api().accept_offer(SdpOffer::from_sdp_string(&restart).unwrap()).unwrap().to_sdp_string());
        core.accept_answer(&answer).expect("we take str0m's restart answer");
        let before = raw.seen().video_frames;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(1) {
            core.send_video(VideoOut { data: fake_h264(n.is_multiple_of(30), n), captured: Instant::now(), rotation: 0 });
            n += 1;
            thread::sleep(Duration::from_millis(33));
        }
        thread::sleep(Duration::from_millis(300));
        assert!(raw.seen().video_frames - before >= 25, "video after the restart: {}", raw.seen().video_frames - before);
    }
}

#[test]
fn str0m_answers_our_offer() {
    str0m_call(true);
}

#[test]
fn we_answer_str0ms_offer() {
    str0m_call(false);
}

// ---------------------------------------------------------------------------------------------
// Mutated descriptions into live connections.

struct Noise(u64);
impl Noise {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn mutate(text: &str, noise: &mut Noise) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    for _ in 0..1 + noise.below(3) {
        if lines.is_empty() {
            break;
        }
        let at = noise.below(lines.len());
        match noise.below(5) {
            0 => {
                lines.remove(at);
            }
            1 => {
                let line = lines[at].clone();
                let to = noise.below(lines.len());
                lines.insert(to, line);
            }
            2 | 3 => {
                const EXTREMES: [&str; 10] = ["0", "1", "15", "64", "96", "127", "128", "255", "65535", "4294967295"];
                let line = lines[at].clone();
                let digits: Vec<usize> = line.char_indices().filter(|(_, c)| c.is_ascii_digit()).map(|(i, _)| i).collect();
                if !digits.is_empty() {
                    let start = digits[noise.below(digits.len())];
                    let end = line[start..].find(|c: char| !c.is_ascii_digit()).map_or(line.len(), |e| start + e);
                    lines[at] = format!("{}{}{}", &line[..start], EXTREMES[noise.below(EXTREMES.len())], &line[end..]);
                }
            }
            _ => {
                let to = noise.below(lines.len());
                lines.swap(at, to);
            }
        }
    }
    lines.join("\r\n") + "\r\n"
}

#[test]
fn mutated_descriptions_never_kill_a_connection() {
    let mut noise = Noise(0x2545_f491_4f6c_dd1d);
    let chrome = include_str!("sdp/chrome-offer.sdp");
    let (source, _) = peer();
    let ours = source.offer(false).unwrap();
    let (answering, _) = peer();
    let answer = answering.accept_offer(&ours).unwrap();
    let (mut taken, mut refused) = (0, 0);
    let mut live = Vec::new();
    for round in 0..240 {
        let (c, events) = peer();
        if round % 3 < 2 {
            let base = if round % 3 == 0 { chrome } else { ours.as_str() };
            match c.accept_offer(&mutate(base, &mut noise)) {
                Ok(_) => taken += 1,
                Err(e) => {
                    assert_eq!(e, Error::Sdp);
                    refused += 1;
                }
            }
        } else {
            // An answer to an offer of a connection, mutated.
            c.offer(false).unwrap();
            let _ = c.accept_answer(&mutate(&answer, &mut noise));
        }
        // The driver lives (a panic in it would end its thread: every call would fail).
        c.channel_open("x", ChannelOptions::default()).expect("the connection is alive");
        assert!(c.stats().is_ok());
        live.push((c, events));
        if live.len() == 40 {
            // Let them run a moment (str0m works on what it was told), then close them at once:
            // a close waits for its reader thread.
            thread::sleep(Duration::from_millis(200));
            thread::scope(|s| {
                for (c, events) in live.drain(..) {
                    assert!(c.stats().is_ok(), "alive after running");
                    s.spawn(move || drop((c, events)));
                }
            });
        }
    }
    eprintln!("mutated offers: {taken} taken, {refused} refused");
    assert!(taken > 10 && refused > 10, "both kinds were tried: {taken} / {refused}");
}
