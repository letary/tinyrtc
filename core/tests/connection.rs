//! Two connections of the library, on real UDP sockets of the loopback interface, in real time:
//! the three signaling calls, video both ways, the data channels, the numbers, the close.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tinyrtc::connection::{ChannelOptions, Config, Connection, Error, Event, Media, State, VideoOut};

fn peer() -> (Connection, Receiver<Event>) {
    let (tx, rx) = mpsc::channel();
    let config = Config { bind: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)), ..Default::default() };
    let connection = Connection::new(config, Box::new(move |e| drop(tx.send(e)))).expect("connection");
    (connection, rx)
}

/// What one side saw.
#[derive(Default, Debug)]
struct Seen {
    states: Vec<State>,
    frames: u32,
    keyframes: u32,
    video_active: bool,
    opened: Vec<(u32, String)>,
    messages: Vec<(u32, Vec<u8>, bool)>,
    closed_channels: Vec<u32>,
    estimates: u32,
}

impl Seen {
    fn take(&mut self, events: &Receiver<Event>) {
        while let Ok(event) = events.try_recv() {
            match event {
                Event::State(s) => self.states.push(s),
                Event::VideoFrame(f) => {
                    self.frames += 1;
                    self.keyframes += f.keyframe as u32;
                    assert_eq!(f.rotation, 90);
                }
                Event::RemoteMedia { media: Media::Video, active } => self.video_active = active,
                Event::ChannelOpen { id, label } => self.opened.push((id, label)),
                Event::ChannelMessage { id, data, binary } => self.messages.push((id, data, binary)),
                Event::ChannelClosed { id } => self.closed_channels.push(id),
                Event::SendEstimate(_) => self.estimates += 1,
                _ => {}
            }
        }
    }
    fn state(&self) -> Option<State> {
        self.states.last().copied()
    }
}

/// An Annex B access unit with real NAL headers and filler below them.
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

fn until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < limit, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_call_between_two_connections() {
    let (a, a_events) = peer();
    let (b, b_events) = peer();
    let (mut sa, mut sb) = (Seen::default(), Seen::default());

    // A channel asked for before any description rides in the first offer.
    let early = a.channel_open("early", ChannelOptions::default()).expect("early channel");

    let offer = a.offer(false).expect("offer");
    assert!(offer.contains("m=audio") && offer.contains("m=video") && offer.contains("m=application"));
    assert_eq!(offer.matches("H264/90000").count(), 1, "one H.264 variant in the profile:\n{offer}");
    assert!(offer.contains("profile-level-id=42e01f"));
    let answer = b.accept_offer(&offer).expect("answer");
    a.accept_answer(&answer).expect("accept answer");
    assert!(a.accept_answer(&answer).is_err(), "an answer with no offer pending is refused");

    until("both connected", Duration::from_secs(10), || {
        sa.take(&a_events);
        sb.take(&b_events);
        sa.state() == Some(State::Connected) && sb.state() == Some(State::Connected)
    });
    assert_eq!(sa.states, [State::Connecting, State::Connected]);
    assert_eq!(sb.states, [State::Connecting, State::Connected]);

    // A channel opened in the middle of the call, from the answering side: no new offer.
    let unreliable = ChannelOptions { unordered: true, max_retransmits: Some(0), ..Default::default() };
    let late = b.channel_open("late", unreliable).expect("late channel");
    until("channels open on both sides", Duration::from_secs(5), || {
        sa.take(&a_events);
        sb.take(&b_events);
        sa.opened.len() == 2 && sb.opened.len() == 2
    });
    let label = |seen: &Seen, label: &str| seen.opened.iter().find(|(_, l)| l == label).map(|(id, _)| *id);
    assert_eq!(label(&sa, "early"), Some(early));
    assert_eq!(label(&sb, "late"), Some(late));
    assert!(sa.opened.iter().chain(&sb.opened).all(|(_, l)| l != "tinyrtc"), "the control channel is not the app's");
    let (early_at_b, late_at_a) = (label(&sb, "early").unwrap(), label(&sa, "late").unwrap());

    // Two seconds of a call: video both ways at 30 fps, messages on both channels.
    let start = Instant::now();
    let mut n = 0u32;
    while start.elapsed() < Duration::from_secs(2) {
        for c in [&a, &b] {
            c.send_video(VideoOut { data: fake_h264(n % 30 == 0, n), captured: Instant::now(), rotation: 90 });
        }
        if n % 3 == 0 {
            a.channel_send(early, format!("a{n}").into_bytes(), false).expect("send early");
            b.channel_send(late, vec![n as u8; 3], true).expect("send late");
        }
        n += 1;
        std::thread::sleep(Duration::from_millis(33));
        sa.take(&a_events);
        sb.take(&b_events);
    }
    std::thread::sleep(Duration::from_millis(300));
    sa.take(&a_events);
    sb.take(&b_events);

    for seen in [&sa, &sb] {
        assert!(seen.frames >= n - 3, "frames {} of {n}", seen.frames);
        assert!(seen.keyframes >= 2, "keyframes {}", seen.keyframes);
        assert!(seen.video_active);
    }
    assert!(sb.messages.iter().all(|(id, data, binary)| *id == early_at_b && !binary && data.starts_with(b"a")));
    assert!(sa.messages.iter().all(|(id, data, binary)| *id == late_at_a && *binary && data.len() == 3));
    assert!(sb.messages.len() as u32 >= n / 3, "text messages {}", sb.messages.len());
    assert!(sa.messages.len() as u32 >= n / 3 - 2, "unreliable messages {}", sa.messages.len());
    assert!(sa.estimates > 0, "no bandwidth estimate");

    // A camera turned off is SAID, not guessed: both stopped sending 300 ms ago; within a second
    // each side knows the other's video stopped, and knows it is back with the first frame.
    until("both see the video stop", Duration::from_secs(2), || {
        sa.take(&a_events);
        sb.take(&b_events);
        !sa.video_active && !sb.video_active
    });
    for c in [&a, &b] {
        c.send_video(VideoOut { data: fake_h264(true, n), captured: Instant::now(), rotation: 90 });
    }
    until("both see the video again", Duration::from_secs(2), || {
        sa.take(&a_events);
        sb.take(&b_events);
        sa.video_active && sb.video_active
    });

    let stats = a.stats().expect("stats");
    assert_eq!(stats.state, State::Connected);
    assert!(stats.video_send_bps > 50_000 && stats.video_recv_bps > 50_000, "{stats:?}");
    assert!(stats.video_recv_fps >= 20, "{stats:?}");
    assert!(stats.bytes_sent > 0 && stats.bytes_received > 0);

    assert_eq!(a.channel_send(9999, vec![1], true), Err(Error::NotFound));

    // Closing a channel is seen on the other side; closing the connection too.
    a.channel_close(early);
    until("channel closed at b", Duration::from_secs(5), || {
        sb.take(&b_events);
        sb.closed_channels == [early_at_b]
    });
    drop(a);
    assert!(a_events.try_iter().any(|e| matches!(e, Event::State(State::Closed))), "the closed side says Closed");
    until("b sees the close", Duration::from_secs(5), || {
        sb.take(&b_events);
        matches!(sb.state(), Some(State::Closed | State::Disconnected))
    });
}

/// A STUN server of one answer: whoever asks is told it is seen as 203.0.113.7:40000.
fn fake_stun_server() -> std::net::SocketAddr {
    use tinyrtc::stun::{attr, Class, Message, MessageWriter, BINDING};
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 1500];
        while let Ok((n, from)) = socket.recv_from(&mut buf) {
            let Ok(request) = Message::parse(&buf[..n]) else { continue };
            let mut response = MessageWriter::new(BINDING, Class::Success, request.transaction_id());
            response.xor_address(attr::XOR_MAPPED_ADDRESS, "203.0.113.7:40000".parse().unwrap());
            let _ = socket.send_to(&response.into_bytes(), from);
        }
    });
    addr
}

#[test]
fn the_address_a_stun_server_sees_is_offered_as_a_candidate() {
    use tinyrtc::connection::IceServer;
    let server = fake_stun_server();
    let (tx, events) = mpsc::channel();
    let config = Config {
        bind: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        ice_servers: vec![
            IceServer { url: format!("stun:127.0.0.1:{}", server.port()), ..Default::default() },
            // A server that never answers costs nothing but its own timeout.
            IceServer { url: "stun:127.0.0.1:9".into(), ..Default::default() },
        ],
        ..Default::default()
    };
    let connection = Connection::new(config, Box::new(move |e| drop(tx.send(e)))).expect("connection");
    let candidate = loop {
        match events.recv_timeout(Duration::from_secs(5)).expect("a reflexive candidate") {
            Event::LocalCandidate(line) => break line,
            _ => continue,
        }
    };
    assert!(candidate.contains("203.0.113.7 40000 typ srflx"), "{candidate}");
    // It is in the description made after it was found, too.
    let offer = connection.offer(false).expect("offer");
    assert!(offer.contains("203.0.113.7 40000 typ srflx"), "{offer}");

    // Relay only: the connection still has its candidates (it sends from them), but no address of
    // ours is ever revealed, not the host one, not the reflexive one: not in an offer or an answer
    // made after the STUN server answered, not as an event.
    let (tx, events) = mpsc::channel();
    let config = Config {
        bind: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        ice_servers: vec![IceServer { url: format!("stun:127.0.0.1:{}", server.port()), ..Default::default() }],
        relay_only: true,
    };
    let hidden = Connection::new(config.clone(), Box::new(move |e| drop(tx.send(e)))).expect("connection");
    std::thread::sleep(Duration::from_millis(300));
    let offer = hidden.offer(false).expect("offer");
    assert!(!offer.contains("a=candidate"), "{offer}");
    assert!(offer.contains("m=audio") && offer.contains("a=ice-ufrag"), "still a whole offer: {offer}");
    assert!(!events.try_iter().any(|e| matches!(e, Event::LocalCandidate(_))));

    let (other, _other_events) = peer();
    let (tx, events) = mpsc::channel();
    let answering = Connection::new(config, Box::new(move |e| drop(tx.send(e)))).expect("connection");
    std::thread::sleep(Duration::from_millis(300));
    let answer = answering.accept_offer(&other.offer(false).unwrap()).expect("answer");
    assert!(!answer.contains("a=candidate") && answer.contains("a=fingerprint"), "{answer}");
    assert!(!events.try_iter().any(|e| matches!(e, Event::LocalCandidate(_))));
}
