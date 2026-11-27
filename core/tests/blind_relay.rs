//! A 1:1 call through a relay that cannot read it. The relay is NOT a TURN server and speaks no
//! protocol at all: it is two UDP ports, and whatever arrives on one leaves from the other towards
//! the address last heard there. Each side is simply told, through signaling, that the other side's
//! candidate is the relay — a `typ relay` candidate, the line the media server hands out. ICE
//! checks, the DTLS handshake and the media all cross it end to end — the relay never holds a key.
//! This is what lets the media server carry a private call without a TURN client in the phone.
//!
//! And what `relay_only` must hold, whatever the app passes it: no candidate of the other side but
//! a relay's is ever checked — added, or inside a description — so the other side never hears
//! from this side's own address, and a call that could go direct goes through the relay. A
//! connection that is not relay-only takes the relay as one candidate among the others, and still
//! goes direct when it can.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tinyrtc::connection::{ChannelOptions, Config, Connection, Event, State, VideoOut};

/// Port `a` faces one peer, port `b` the other. Returns the two addresses and a datagram counter.
fn blind_relay() -> (SocketAddr, SocketAddr, Arc<AtomicU64>) {
    let a = UdpSocket::bind("127.0.0.1:0").unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").unwrap();
    let (addr_a, addr_b) = (a.local_addr().unwrap(), b.local_addr().unwrap());
    let forwarded = Arc::new(AtomicU64::new(0));
    // The address each port last heard from: where the other port's traffic goes.
    let heard: [Arc<Mutex<Option<SocketAddr>>>; 2] = [Arc::default(), Arc::default()];
    for (i, (listen, send)) in [(a.try_clone().unwrap(), b.try_clone().unwrap()), (b, a)].into_iter().enumerate() {
        let (mine, other) = (heard[i].clone(), heard[1 - i].clone());
        let forwarded = forwarded.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 2000];
            loop {
                let Ok((n, from)) = listen.recv_from(&mut buf) else { continue };
                *mine.lock().unwrap() = Some(from);
                // Bytes in, bytes out: nothing is parsed, nothing could be.
                if let Some(to) = *other.lock().unwrap() {
                    let _ = send.send_to(&buf[..n], to);
                    forwarded.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
    }
    (addr_a, addr_b, forwarded)
}

/// An address of "the other side" that only listens: what it hears, counted.
fn sink() -> (SocketAddr, Arc<AtomicU64>) {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    let heard = Arc::new(AtomicU64::new(0));
    let count = heard.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 2000];
        loop {
            if socket.recv_from(&mut buf).is_ok() {
                count.fetch_add(1, Ordering::Relaxed);
            }
        }
    });
    (addr, heard)
}

/// The media server's relay candidate (mediaserver/src/call.rs): `typ relay`, the lowest priority.
fn relay_line(addr: SocketAddr) -> String {
    format!("candidate:1 1 udp 255 {} {} typ relay raddr 0.0.0.0 rport 0", addr.ip(), addr.port())
}

/// A candidate of the other side's own, of a kind (host / srflx / prflx), as an app could pass it.
fn own_line(kind: &str, addr: SocketAddr) -> String {
    let (priority, related) = match kind {
        "host" => (2130706431u32, ""),
        "prflx" => (1853824767, " raddr 0.0.0.0 rport 0"),
        _ => (1694498815, " raddr 0.0.0.0 rport 0"),
    };
    format!("candidate:7 1 udp {priority} {} {} typ {kind}{related}", addr.ip(), addr.port())
}

/// A description with these candidate lines put into its first section (after its own, if any).
fn with_candidates(sdp: &str, lines: &[String]) -> String {
    let mut out: Vec<String> = sdp.split("\r\n").map(str::to_owned).collect();
    let at = out
        .iter()
        .rposition(|l| l.starts_with("a=candidate:"))
        .or_else(|| out.iter().position(|l| l.starts_with("a=mid:")))
        .expect("a section");
    for (i, line) in lines.iter().enumerate() {
        out.insert(at + 1 + i, format!("a={line}"));
    }
    out.join("\r\n")
}

fn peer(relay_only: bool) -> (Connection, Receiver<Event>) {
    let (tx, rx) = mpsc::channel();
    let config = Config { bind: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)), relay_only, ..Default::default() };
    (Connection::new(config, Box::new(move |e| drop(tx.send(e)))).expect("connection"), rx)
}

#[derive(Default, Debug)]
struct Seen {
    states: Vec<State>,
    frames: u32,
    messages: u32,
    local_candidates: Vec<String>,
}

impl Seen {
    fn take(&mut self, events: &Receiver<Event>) {
        for e in events.try_iter() {
            match e {
                Event::State(s) => self.states.push(s),
                Event::VideoFrame(_) => self.frames += 1,
                Event::ChannelMessage { .. } => self.messages += 1,
                Event::LocalCandidate(line) => self.local_candidates.push(line),
                _ => {}
            }
        }
    }
    fn connected(&self) -> bool {
        self.states.last() == Some(&State::Connected)
    }
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

/// Both sides connected within `limit`.
fn connect(pair: [(&Receiver<Event>, &mut Seen); 2], limit: Duration) -> Duration {
    let [(a_events, a), (b_events, b)] = pair;
    let start = Instant::now();
    while !(a.connected() && b.connected()) {
        assert!(start.elapsed() < limit, "not connected: {:?} / {:?}", a.states, b.states);
        std::thread::sleep(Duration::from_millis(5));
        a.take(a_events);
        b.take(b_events);
    }
    start.elapsed()
}

/// Video both ways at 30 fps for `how_long`; returns the frames each side sent.
fn video(a: &Connection, b: &Connection, how_long: Duration, mut tick: impl FnMut(u32)) -> u32 {
    let start = Instant::now();
    let mut n = 0u32;
    while start.elapsed() < how_long {
        for c in [a, b] {
            c.send_video(VideoOut { data: fake_h264(n % 30 == 0, n), captured: Instant::now(), rotation: 0 });
        }
        tick(n);
        n += 1;
        std::thread::sleep(Duration::from_millis(33));
    }
    std::thread::sleep(Duration::from_millis(300));
    n
}

#[test]
fn a_call_crosses_a_relay_that_holds_no_key() {
    let (relay_a, relay_b, forwarded) = blind_relay();
    let ((a, a_events), (b, b_events)) = (peer(true), peer(true));
    let (mut sa, mut sb) = (Seen::default(), Seen::default());

    let chat = a.channel_open("chat", ChannelOptions::default()).unwrap();
    let offer = a.offer(false).unwrap();
    let answer = b.accept_offer(&offer).unwrap();
    a.accept_answer(&answer).unwrap();
    assert!(!offer.contains("a=candidate") && !answer.contains("a=candidate"));

    // All each side knows of the other: "it is at the relay".
    a.add_candidate(&relay_line(relay_a));
    b.add_candidate(&relay_line(relay_b));

    let connected_in = connect([(&a_events, &mut sa), (&b_events, &mut sb)], Duration::from_secs(10));
    let n = video(&a, &b, Duration::from_secs(2), |n| {
        if n % 3 == 0 {
            let _ = a.channel_send(chat, b"hello".to_vec(), false);
        }
        sa.take(&a_events);
        sb.take(&b_events);
    });
    sa.take(&a_events);
    sb.take(&b_events);

    let through = forwarded.load(Ordering::Relaxed);
    eprintln!("connected in {connected_in:?}; frames a {} b {} of {n}; messages {}; datagrams through the relay {through}", sa.frames, sb.frames, sb.messages);
    assert!(sa.frames >= n - 3 && sb.frames >= n - 3, "video both ways: {} / {} of {n}", sa.frames, sb.frames);
    assert!(sb.messages >= n / 3 - 1, "the data channel too: {}", sb.messages);
    // It really went through: the peers were given no other address of each other.
    assert!(through > (n as u64) * 2, "only {through} datagrams crossed the relay");
}

/// The flaw this guards: a relay-only connection used to hide its own candidates but still check
/// the other side's, so the two met directly and the other side learned the hidden address from
/// the checks. Given the other side's own candidates of every kind — trickled before and after the
/// descriptions, and inside the other side's description — and no relay, it must never connect,
/// and no address it was given may hear a single datagram from it. Offering and answering.
#[test]
fn relay_only_checks_no_candidate_but_a_relay() {
    for relay_only_offers in [false, true] {
        let ((r, r_events), (o, o_events)) = (peer(true), peer(false));
        let (mut sr, mut so) = (Seen::default(), Seen::default());
        let sinks: Vec<(SocketAddr, Arc<AtomicU64>)> = (0..6).map(|_| sink()).collect();
        let kinds = ["host", "srflx", "prflx"];
        // In the description: the other side's own host candidate (it is not relay only) and three more.
        let described: Vec<String> = (0..3).map(|i| own_line(kinds[i], sinks[i].0)).collect();

        r.add_candidate(&own_line("host", sinks[3].0)); // before any description
        if relay_only_offers {
            let offer = r.offer(false).unwrap();
            let answer = o.accept_offer(&offer).unwrap();
            assert!(answer.contains("a=candidate:"), "the plain side's answer carries its address: {answer}");
            r.accept_answer(&with_candidates(&answer, &described)).unwrap();
        } else {
            let offer = o.offer(false).unwrap();
            assert!(offer.contains("a=candidate:"), "the plain side's offer carries its address: {offer}");
            let answer = r.accept_offer(&with_candidates(&offer, &described)).unwrap();
            assert!(!answer.contains("a=candidate"), "{answer}");
            o.accept_answer(&answer).unwrap();
        }
        r.add_candidate(&own_line("srflx", sinks[4].0));
        r.add_candidate(&own_line("prflx", sinks[5].0));

        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(20));
            sr.take(&r_events);
            so.take(&o_events);
            assert!(!sr.states.contains(&State::Connected) && !so.states.contains(&State::Connected),
                "relay only, no relay: it connected anyway ({:?} / {:?})", sr.states, so.states);
        }
        let heard: Vec<u64> = sinks.iter().map(|(_, n)| n.load(Ordering::Relaxed)).collect();
        eprintln!("relay only {}: states {:?} / {:?}; datagrams to the addresses it was given {heard:?}",
            if relay_only_offers { "offering" } else { "answering" }, sr.states, so.states);
        assert!(heard.iter().all(|n| *n == 0), "the relay-only side sent to an address of the other side: {heard:?}");
        assert!(sr.local_candidates.is_empty(), "{:?}", sr.local_candidates);
    }
}

/// The relay-only side is given everything — the other side's own address in its description and
/// trickled, and the relay — and goes through the relay only; an ICE restart (the other side's new
/// offer carries its address again) reveals nothing and changes nothing.
#[test]
fn relay_only_given_every_candidate_goes_through_the_relay_only() {
    let (relay_r, relay_o, forwarded) = blind_relay();
    let ((r, r_events), (o, o_events)) = (peer(true), peer(false));
    let (mut sr, mut so) = (Seen::default(), Seen::default());
    let (trickled, trickled_heard) = sink();
    let (described, described_heard) = sink();

    let offer = o.offer(false).unwrap();
    assert!(offer.contains("a=candidate:"), "the plain side offers its address: {offer}");
    let answer = r.accept_offer(&with_candidates(&offer, &[own_line("host", described)])).unwrap();
    assert!(!answer.contains("a=candidate"), "{answer}");
    o.accept_answer(&answer).unwrap();
    r.add_candidate(&own_line("host", trickled));
    r.add_candidate(&relay_line(relay_r));
    o.add_candidate(&relay_line(relay_o));

    let connected_in = connect([(&r_events, &mut sr), (&o_events, &mut so)], Duration::from_secs(10));
    let before = forwarded.load(Ordering::Relaxed);
    let n = video(&r, &o, Duration::from_secs(2), |_| {
        sr.take(&r_events);
        so.take(&o_events);
    });
    sr.take(&r_events);
    so.take(&o_events);
    let through = forwarded.load(Ordering::Relaxed) - before;
    eprintln!("connected in {connected_in:?}; frames r {} o {} of {n}; datagrams through the relay while sending {through}", sr.frames, so.frames);
    assert!(sr.frames >= n - 3 && so.frames >= n - 3, "video both ways: {} / {} of {n}", sr.frames, so.frames);
    assert!(through > (n as u64) * 2, "the media went around the relay: only {through} datagrams crossed it");

    // The offering side restarts ICE: its new offer carries its address again, the relay-only
    // answer carries none; the call goes on, through the relay.
    let restart = o.offer(true).unwrap();
    assert!(restart.contains("a=candidate:"), "{restart}");
    let answer = r.accept_offer(&restart).unwrap();
    assert!(!answer.contains("a=candidate") && answer.contains("a=ice-ufrag"), "{answer}");
    o.accept_answer(&answer).unwrap();
    let (frames_r, frames_o) = (sr.frames, so.frames);
    let before = forwarded.load(Ordering::Relaxed);
    let n = video(&r, &o, Duration::from_secs(2), |_| {
        sr.take(&r_events);
        so.take(&o_events);
    });
    sr.take(&r_events);
    so.take(&o_events);
    let through = forwarded.load(Ordering::Relaxed) - before;
    eprintln!("after the restart: frames r {} o {} of {n}; through the relay {through}", sr.frames - frames_r, so.frames - frames_o);
    assert!(sr.frames - frames_r >= n - 5 && so.frames - frames_o >= n - 5, "after the restart");
    assert!(through > (n as u64) * 2, "after the restart, only {through} datagrams crossed the relay");
    assert!(sr.connected() && so.connected(), "{:?} / {:?}", sr.states, so.states);

    let heard = [trickled_heard.load(Ordering::Relaxed), described_heard.load(Ordering::Relaxed)];
    assert_eq!(heard, [0, 0], "the relay-only side sent to an address of the other side");
    assert!(sr.local_candidates.is_empty(), "relay only raised a candidate: {:?}", sr.local_candidates);
}

/// Not relay only: the relay's candidate is one among the others, the last choice — a call that
/// can go direct still does (and the relay saw the checks: the candidate was taken).
#[test]
fn a_plain_connection_given_the_relay_too_still_goes_direct() {
    let (relay_a, relay_b, forwarded) = blind_relay();
    let ((a, a_events), (b, b_events)) = (peer(false), peer(false));
    let (mut sa, mut sb) = (Seen::default(), Seen::default());
    let offer = a.offer(false).unwrap();
    let answer = b.accept_offer(&offer).unwrap();
    a.accept_answer(&answer).unwrap();
    assert!(offer.contains("a=candidate:") && answer.contains("a=candidate:"));
    a.add_candidate(&relay_line(relay_a));
    b.add_candidate(&relay_line(relay_b));

    let connected_in = connect([(&a_events, &mut sa), (&b_events, &mut sb)], Duration::from_secs(10));
    std::thread::sleep(Duration::from_millis(500));
    let before = forwarded.load(Ordering::Relaxed);
    let n = video(&a, &b, Duration::from_secs(2), |_| {
        sa.take(&a_events);
        sb.take(&b_events);
    });
    sa.take(&a_events);
    sb.take(&b_events);
    let (total, during) = (forwarded.load(Ordering::Relaxed), forwarded.load(Ordering::Relaxed) - before);
    eprintln!("connected in {connected_in:?}; frames a {} b {} of {n}; through the relay: {total} in all, {during} while sending", sa.frames, sb.frames);
    assert!(sa.frames >= n - 3 && sb.frames >= n - 3, "video both ways: {} / {} of {n}", sa.frames, sb.frames);
    assert!(during < n as u64 / 4, "the media went through the relay: {during} datagrams while sending {n} frames each way");
    assert!(total > 0, "the relay's candidate was not even checked");
}
