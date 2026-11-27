//! Calls, end to end: two relay-only phones whose descriptions carry no address, the server's
//! public address as each one's only candidate (a `typ relay` one), the server forwarding bytes it
//! cannot read. And a call where only one side is relay-only: the other side's own candidates
//! reach it too, and it goes through the server all the same.

mod common;

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use common::*;
use tinyrtc::connection::{ChannelOptions, State};
use tinyrtc::stun::{attr, Class, MessageWriter, TransactionId, BINDING};
use tinyrtc_mediaserver::Server;

/// The backend's part of a call: the descriptions cross, the ufrags are reserved, both phones
/// get the candidate (rewritten to a phone's NAT when it has one). Returns the call's id.
fn call(server: &Server, a: &Phone, b: &Phone, nats: [Option<&Proxy>; 2]) -> (String, [String; 2]) {
    let offer = a.conn.offer(false).unwrap();
    let answer = b.conn.accept_offer(&offer).unwrap();
    a.conn.accept_answer(&answer).unwrap();
    assert!(!offer.contains("a=candidate") && !answer.contains("a=candidate"), "relay only: no address in a description");
    let ufrags = [ufrag_of(&offer), ufrag_of(&answer)];
    let (status, body) = http(server, "POST", "/calls", &format!("{{\"ufrags\": [\"{}\", \"{}\"]}}", ufrags[0], ufrags[1]));
    assert_eq!(status, 201, "{body}");
    let id = field(&body, "id").unwrap().to_owned();
    let candidate = field(&body, "candidate").unwrap().to_owned();
    let public = server.public_addr();
    assert_eq!(candidate, relay_line(public));
    for (phone, nat) in [a, b].into_iter().zip(nats) {
        let line = match nat {
            Some(nat) => candidate.replace(&format!(" {} {} ", public.ip(), public.port()), &format!(" {} {} ", nat.addr.ip(), nat.addr.port())),
            None => candidate.clone(),
        };
        phone.conn.add_candidate(&line);
    }
    (id, ufrags)
}

/// The line POST /calls answers: the server's address, `typ relay`, the lowest priority.
fn relay_line(public: SocketAddr) -> String {
    format!("candidate:1 1 udp 255 {} {} typ relay raddr 0.0.0.0 rport 0", public.ip(), public.port())
}

fn connect(a: &mut Phone, b: &mut Phone) {
    until("both phones connected through the server", Duration::from_secs(10), || {
        a.pump();
        b.pump();
        a.connected() && b.connected()
    });
}

/// A STUN binding request as a phone's ICE check looks, with any USERNAME (or none). The
/// MESSAGE-INTEGRITY is made up: the server cannot check it.
fn check(username: Option<&str>) -> Vec<u8> {
    let mut w = MessageWriter::new(BINDING, Class::Request, TransactionId::random(Instant::now()));
    if let Some(name) = username {
        w.attribute(attr::USERNAME, name.as_bytes());
    }
    w.attribute(0x0024, &[0x6e, 0, 1, 0xff]); // PRIORITY
    w.attribute(attr::MESSAGE_INTEGRITY, &[0x5a; 20]).fingerprint();
    w.into_bytes()
}

/// Everything the socket receives within `window`.
fn received(socket: &UdpSocket, window: Duration) -> usize {
    socket.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
    let (start, mut n, mut buf) = (Instant::now(), 0, [0u8; 2048]);
    while start.elapsed() < window {
        if socket.recv_from(&mut buf).is_ok() {
            n += 1;
        }
    }
    n
}

fn sides(server: &Server, id: &str) -> String {
    let (status, body) = http(server, "GET", &format!("/calls/{id}"), "");
    assert_eq!(status, 200, "{body}");
    body
}

/// (packets, bytes) of a side in GET /calls/{id}.
fn counted(body: &str, ufrag: &str) -> (u64, u64) {
    let at = body.find(&format!("\"ufrag\":\"{ufrag}\"")).unwrap();
    let side = &body[at..at + body[at..].find('}').unwrap()];
    let number = |key: &str| -> u64 {
        let from = side.find(&format!("\"{key}\":")).unwrap() + key.len() + 3;
        side[from..].split(',').next().unwrap().trim_end_matches('}').parse().unwrap()
    };
    (number("packets"), number("bytes"))
}

#[test]
fn a_call_crosses_the_server_blind() {
    let (server, log) = server(|_| {});
    let (mut a, mut b) = (Phone::new("a", true), Phone::new("b", true));
    let chat = a.conn.channel_open("chat", ChannelOptions::default()).unwrap();
    let (id, ufrags) = call(&server, &a, &b, [None, None]);
    let body = sides(&server, &id);
    assert!(body.contains("\"connected\":false") && !body.contains("\"connected\":true"), "nobody latched yet: {body}");
    connect(&mut a, &mut b);

    let start = Instant::now();
    let mut messages = 0;
    while start.elapsed() < Duration::from_secs(2) {
        a.send_due();
        b.send_due();
        let open = a.seen.opened.iter().any(|(_, l)| l == "chat");
        if open && messages < start.elapsed().as_millis() / 100 && a.conn.channel_send(chat, format!("hello {messages}").into_bytes(), false).is_ok() {
            messages += 1;
        }
        a.pump();
        b.pump();
        std::thread::sleep(Duration::from_millis(3));
    }
    std::thread::sleep(Duration::from_millis(300));
    a.pump();
    b.pump();
    eprintln!(
        "frames a <- b {} of {}, b <- a {} of {}; audio {} / {}; messages {} of {messages}",
        a.seen.frames, b.sent_frames(), b.seen.frames, a.sent_frames(), a.seen.audio, b.seen.audio, b.seen.messages.len()
    );
    assert!(a.seen.frames + 3 >= b.sent_frames() && b.seen.frames + 3 >= a.sent_frames(), "video both ways");
    assert!(a.seen.audio + 10 >= b.sent_audio() && b.seen.audio + 10 >= a.sent_audio(), "audio both ways");
    assert!(messages >= 10 && b.seen.messages.len() as u128 >= messages - 1, "the data channel: {} of {messages}", b.seen.messages.len());
    assert!(b.seen.messages.iter().all(|m| m.starts_with(b"hello ")));

    let body = sides(&server, &id);
    eprintln!("{body}");
    assert!(!body.contains("\"connected\":false"), "{body}");
    for ufrag in &ufrags {
        let (packets, bytes) = counted(&body, ufrag);
        assert!(packets > 150 && bytes > 40_000, "{ufrag}: {packets} datagrams, {bytes} bytes: {body}");
    }
    let mut seen = Vec::new();
    lines(&log, &mut seen);
    assert!(seen.iter().any(|l| l.contains("side 0 latched")) && seen.iter().any(|l| l.contains("side 1 latched")), "{seen:#?}");
}

/// Only the callee is relay-only. The caller is a plain connection: its offer carries its address,
/// and the app trickles its candidates to the callee as it would in any call — the callee checks
/// none of them, so the call crosses the server anyway, and the caller never hears from the
/// callee's own address (the server is the only one that sends to it).
#[test]
fn only_one_side_relay_only_still_crosses_the_server() {
    let (server, _log) = server(|_| {});
    let (mut a, mut b) = (Phone::new("caller", false), Phone::new("callee", true));
    let offer = a.conn.offer(false).unwrap();
    let own: Vec<String> = offer.lines().filter_map(|l| l.strip_prefix("a=")).filter(|l| l.starts_with("candidate:")).map(str::to_owned).collect();
    assert!(!own.is_empty(), "the plain caller's offer carries its address: {offer}");
    let answer = b.conn.accept_offer(&offer).unwrap();
    a.conn.accept_answer(&answer).unwrap();
    assert!(!answer.contains("a=candidate"), "{answer}");
    for line in &own {
        b.conn.add_candidate(line); // trickled too, as an app forwards every candidate
    }
    let (status, body) = http(&server, "POST", "/calls", &format!("{{\"ufrags\": [\"{}\", \"{}\"]}}", ufrag_of(&offer), ufrag_of(&answer)));
    assert_eq!(status, 201, "{body}");
    let (id, candidate) = (field(&body, "id").unwrap().to_owned(), field(&body, "candidate").unwrap().to_owned());
    a.conn.add_candidate(&candidate);
    b.conn.add_candidate(&candidate);
    connect(&mut a, &mut b);
    let (status, before) = http(&server, "GET", &format!("/calls/{id}"), "");
    assert_eq!(status, 200, "{before}");
    run_for(Duration::from_secs(2), &mut [&mut a, &mut b], &mut []);
    let body = sides(&server, &id);
    eprintln!("frames caller <- callee {} of {}, callee <- caller {} of {}; {body}", a.seen.frames, b.sent_frames(), b.seen.frames, a.sent_frames());
    assert!(a.seen.frames + 3 >= b.sent_frames() && b.seen.frames + 3 >= a.sent_frames(), "video both ways");
    for ufrag in [ufrag_of(&offer), ufrag_of(&answer)] {
        let (packets, bytes) = counted(&body, &ufrag);
        assert!(packets > 150 && bytes > 40_000, "{ufrag}: {packets} datagrams, {bytes} bytes went through the server: {body}");
    }
}

#[test]
fn only_both_ufrags_latch_a_side() {
    let (server, log) = server(|_| {});
    let (mut a, mut b) = (Phone::new("a", true), Phone::new("b", true));
    let (id, [ua, ub]) = call(&server, &a, &b, [None, None]);
    connect(&mut a, &mut b);
    run_for(Duration::from_millis(300), &mut [&mut a, &mut b], &mut []);

    // A third phone that knows one ufrag, or none: its checks, and anything else it sends.
    let mallory = UdpSocket::bind("127.0.0.1:0").unwrap();
    let to = server.udp_addr();
    let names = [
        format!("{ua}:mallory"),
        format!("mallory:{ua}"),
        format!("{ub}:mallory"),
        format!("mallory:{ub}"),
        format!("{ua}:{ua}"),
        format!("{ub}:{ub}"),
        ua.to_string(),
        format!("{ua}:{ub}x"),
        format!("x{ub}:{ua}"),
        "mallory:mallet".to_owned(),
    ];
    let mut rtp = vec![0x80, 0x6f, 0x12, 0x34, 0, 0, 0x10, 0, 0xde, 0xad, 0xbe, 0xef];
    rtp.extend_from_slice(&[0x55; 160]);
    let (frames_a, frames_b) = (a.seen.frames, b.seen.frames);
    let start = Instant::now();
    let mut got = 0;
    while start.elapsed() < Duration::from_millis(1500) {
        for name in &names {
            mallory.send_to(&check(Some(name)), to).unwrap();
        }
        mallory.send_to(&check(None), to).unwrap();
        mallory.send_to(&rtp, to).unwrap();
        mallory.send_to(&[0x16, 0xfe, 0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x30, 0x01], to).unwrap();
        got += received(&mallory, Duration::from_millis(50));
        run_for(Duration::from_millis(10), &mut [&mut a, &mut b], &mut []);
    }
    assert_eq!(got, 0, "the server forwarded something to a phone that knows less than both ufrags");
    assert!(a.connected() && b.connected());
    assert!(a.seen.frames > frames_a + 20 && b.seen.frames > frames_b + 20, "the call goes on");
    let mut seen = Vec::new();
    lines(&log, &mut seen);
    assert_eq!(seen.iter().filter(|l| l.contains("latched")).count(), 2, "only the two phones ever latched: {seen:#?}");

    // The limit (call.rs): MESSAGE-INTEGRITY is not checked, so whoever knows BOTH ufrags can
    // take a side's place — here b's ("<a's ufrag>:<b's>" is what b sends) — until b's next
    // check takes it back. What it gets is SRTP / DTLS it holds no key for.
    mallory.send_to(&check(Some(&format!("{ua}:{ub}"))), to).unwrap();
    let start = Instant::now();
    let mut hijacked = 0;
    while hijacked == 0 && start.elapsed() < Duration::from_secs(2) {
        run_for(Duration::from_millis(10), &mut [&mut a, &mut b], &mut []);
        hijacked += received(&mallory, Duration::from_millis(30));
    }
    assert!(hijacked > 0, "both ufrags latch, as documented");
    lines(&log, &mut seen);
    assert!(seen.iter().any(|l| l.contains("side 1 re-latched")), "{seen:#?}");
    let _ = id;
}

#[test]
fn a_side_that_moves_is_relatched() {
    let (server, log) = server(|_| {});
    let nat = Proxy::new(server.udp_addr());
    let (mut a, mut b) = (Phone::new("a", true), Phone::new("b", true));
    call(&server, &a, &b, [None, Some(&nat)]);
    connect(&mut a, &mut b);
    run_for(Duration::from_millis(500), &mut [&mut a, &mut b], &mut []);

    // b's NAT gives it a new outward port: the server sees b at a new address.
    let (before_a, before_b) = (a.seen.frames, b.seen.frames);
    let old = nat.outward_addr();
    assert_ne!(nat.rebind(), old);
    let moved = Instant::now();
    while !(a.seen.frames > before_a + 30 && b.seen.frames > before_b + 30) {
        assert!(moved.elapsed() < Duration::from_secs(10), "after the move: a {} -> {}, b {} -> {}", before_a, a.seen.frames, before_b, b.seen.frames);
        run_for(Duration::from_millis(20), &mut [&mut a, &mut b], &mut []);
    }
    eprintln!("both ways again {:?} after b moved", moved.elapsed());
    let mut seen = Vec::new();
    lines(&log, &mut seen);
    assert!(seen.iter().any(|l| l.contains("side 1 re-latched")), "{seen:#?}");
    assert!(a.connected() && b.connected());
}

#[test]
fn an_idle_call_is_released_and_its_phones_cannot_get_through() {
    released_call(false);
}

/// The same, waited out until both phones say Disconnected: str0m's checks on the phone give a
/// silent path up only after ~20 s (seen: 21 s), too long for the default run.
#[test]
#[ignore = "slow: ~25 s"]
fn an_idle_call_is_released_and_its_phones_go_disconnected() {
    released_call(true);
}

fn released_call(until_disconnected: bool) {
    let (server, log) = server(|c| c.call_idle = Duration::from_millis(1500));
    let (nat_a, nat_b) = (Proxy::new(server.udp_addr()), Proxy::new(server.udp_addr()));
    let (mut a, mut b) = (Phone::new("a", true), Phone::new("b", true));
    let (id, _) = call(&server, &a, &b, [Some(&nat_a), Some(&nat_b)]);
    connect(&mut a, &mut b);
    run_for(Duration::from_millis(500), &mut [&mut a, &mut b], &mut []);

    // Both networks go silent: no datagram reaches the server.
    nat_a.cut(true);
    nat_b.cut(true);
    let cut = Instant::now();
    let states = (a.seen.states.len(), b.seen.states.len());
    until("the call is released", Duration::from_secs(4), || {
        run_for(Duration::from_millis(20), &mut [&mut a, &mut b], &mut []);
        http(&server, "GET", &format!("/calls/{id}"), "").0 == 404
    });
    let released = cut.elapsed();
    eprintln!("released {released:?} after the last datagram");
    assert!(released >= Duration::from_millis(1400), "{released:?}");
    let mut seen = Vec::new();
    lines(&log, &mut seen);
    assert!(seen.iter().any(|l| l.contains("released: idle")), "{seen:#?}");

    // The networks come back, the call does not: nothing is latched, nothing reserved. The phones
    // keep trying; the server answers nothing.
    let before = [nat_a.passed(), nat_b.passed()];
    nat_a.cut(false);
    nat_b.cut(false);
    run_for(Duration::from_millis(2000), &mut [&mut a, &mut b], &mut []);
    for (nat, before) in [&nat_a, &nat_b].into_iter().zip(before) {
        let now = nat.passed();
        assert!(now[0] > before[0], "the phone still sends: {before:?} -> {now:?}");
        assert_eq!(now[1], before[1], "the server sent a released call's phone something");
    }
    if !until_disconnected {
        return;
    }
    until("both phones disconnected", Duration::from_secs(30), || {
        run_for(Duration::from_millis(20), &mut [&mut a, &mut b], &mut []);
        matches!(a.state(), Some(State::Disconnected | State::Failed)) && matches!(b.state(), Some(State::Disconnected | State::Failed))
    });
    eprintln!("both phones {:?} / {:?} {:?} after the cut", a.state(), b.state(), cut.elapsed());
    run_for(Duration::from_millis(3000), &mut [&mut a, &mut b], &mut []);
    for (phone, from) in [(&a, states.0), (&b, states.1)] {
        assert!(!phone.seen.states[from..].contains(&State::Connected), "{} came back: {:?}", phone.name, phone.seen.states);
    }
}

#[test]
fn a_phone_dropped_before_its_peer_latches_gets_nothing_back() {
    // A side alone: what it sends is dropped (the other side has no address), never echoed.
    let (server, _log) = server(|_| {});
    let lone = UdpSocket::bind("127.0.0.1:0").unwrap();
    let (status, body) = http(&server, "POST", "/calls", "{\"ufrags\":[\"aaaa\",\"bbbb\"]}");
    assert_eq!(status, 201, "{body}");
    let id = field(&body, "id").unwrap().to_owned();
    let to: SocketAddr = server.udp_addr();
    lone.send_to(&check(Some("aaaa:bbbb")), to).unwrap();
    lone.send_to(&[0x80; 100], to).unwrap();
    assert_eq!(received(&lone, Duration::from_millis(300)), 0);
    let body = sides(&server, &id);
    assert!(body.contains("{\"ufrag\":\"aaaa\",\"connected\":false,\"packets\":0,\"bytes\":0}"), "{body}");
    assert!(body.contains("{\"ufrag\":\"bbbb\",\"connected\":true,\"packets\":2,"), "{body}");
}
