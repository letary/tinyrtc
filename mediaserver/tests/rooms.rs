//! Rooms, end to end: the server in-process on loopback, phones = core's `Connection`, the
//! backend's requests over HTTP, fake H.264 and Opus, real time.

mod common;

use std::time::{Duration, Instant};

use common::*;
use tinyrtc::connection::State;

/// Publishers and a subscribe connection for every ordered pair.
struct Mesh {
    room: String,
    publishers: Vec<Phone>,
    /// (subscriber index, publisher index, phone)
    subscriptions: Vec<(usize, usize, Phone)>,
}

impl Mesh {
    fn new(server: &tinyrtc_mediaserver::Server, names: &[&str]) -> Mesh {
        let room = create_room(server);
        let publishers: Vec<Phone> = names.iter().map(|name| Phone::new(name, false)).collect();
        for p in &publishers {
            join(server, p, &format!("/rooms/{room}/publish/{}", p.name), None);
        }
        let mut subscriptions = Vec::new();
        for (i, me) in names.iter().enumerate() {
            for (j, other) in names.iter().enumerate() {
                if i != j {
                    let phone = Phone::new(&format!("{me}<-{other}"), false);
                    join(server, &phone, &format!("/rooms/{room}/subscribe/{me}/{other}"), None);
                    subscriptions.push((i, j, phone));
                }
            }
        }
        let mut mesh = Mesh { room, publishers, subscriptions };
        until("every connection of the mesh connected", Duration::from_secs(10), || {
            mesh.pump();
            mesh.publishers.iter().all(Phone::connected) && mesh.subscriptions.iter().all(|(_, _, p)| p.connected())
        });
        mesh
    }

    fn pump(&mut self) {
        self.publishers.iter_mut().for_each(Phone::pump);
        self.subscriptions.iter_mut().for_each(|(_, _, p)| p.pump());
    }

    fn run(&mut self, how_long: Duration) {
        let mut publishers: Vec<&mut Phone> = self.publishers.iter_mut().collect();
        let mut others: Vec<&mut Phone> = self.subscriptions.iter_mut().map(|(_, _, p)| p).collect();
        run_for(how_long, &mut publishers, &mut others);
    }

    fn subscription(&self, me: usize, other: usize) -> &Phone {
        &self.subscriptions.iter().find(|(i, j, _)| *i == me && *j == other).expect("a subscription").2
    }
}

#[test]
fn three_participants_see_and_hear_each_other_and_a_late_joiner_gets_a_keyframe() {
    let (server, _log) = server(|_| {});
    let names = ["alice", "bob", "carol"];
    let mut mesh = Mesh::new(&server, &names);
    mesh.run(Duration::from_millis(2500));
    // Frames still in flight when the senders stop.
    std::thread::sleep(Duration::from_millis(300));
    mesh.pump();

    for (i, j, phone) in &mesh.subscriptions {
        let publisher = &mesh.publishers[*j];
        let (sent, got) = (publisher.sent_frames(), phone.seen.frames);
        eprintln!(
            "{} <- {}: video {got} of {sent} ({} keyframes), audio {} of {}",
            names[*i], names[*j], phone.seen.keyframes, phone.seen.audio, publisher.sent_audio()
        );
        // The first frames can fall before the server's side of the subscription is up.
        assert!(got + 5 >= sent && got <= sent, "{} <- {}: {got} frames of {sent}", names[*i], names[*j]);
        assert!(phone.seen.keyframes >= 3, "{} <- {}: {} keyframes", names[*i], names[*j], phone.seen.keyframes);
        assert_eq!(phone.seen.rotations_wrong, 0, "the orientation crosses the server");
        assert!(phone.seen.audio + 10 >= publisher.sent_audio(), "{} <- {}: {} audio packets of {}", names[*i], names[*j], phone.seen.audio, publisher.sent_audio());
    }

    let (status, body) = http(&server, "GET", &format!("/rooms/{}", mesh.room), "");
    assert_eq!(status, 200, "{body}");
    for (i, name) in names.iter().enumerate() {
        let p = participant(&body, name).unwrap_or_else(|| panic!("{name} in {body}"));
        let others: Vec<String> = names.iter().filter(|n| *n != name).map(|n| format!("\"{n}\"")).collect();
        assert!(p.contains("\"publishing\":true,\"connected\":true"), "{p}");
        assert!(p.contains(&format!("\"subscribers\":[{}]", others.join(","))), "{p}");
        let bytes = |key: &str| -> u64 {
            let at = p.find(&format!("\"{key}\":")).unwrap() + key.len() + 3;
            p[at..].trim_end_matches('}').split(',').next().unwrap().parse().unwrap()
        };
        // In: its own media (~2.5 s of ~25 kB/s); out: the two others'.
        assert!(bytes("bytes_in") > 50_000 && bytes("bytes_out") > 100_000, "{} {p}", names[i]);
    }

    // A late joiner: alice is asked for a keyframe as soon as dave's subscription is up, and
    // dave's first picture is one.
    let asked_before = mesh.publishers[0].seen.keyframe_requests;
    let mut dave = Phone::new("dave<-alice", false);
    join(&server, &dave, &format!("/rooms/{}/subscribe/dave/alice", mesh.room), None);
    let (mut connected_at, mut asked_at) = (None, None);
    let start = Instant::now();
    while dave.seen.frames < 10 {
        assert!(start.elapsed() < Duration::from_secs(5), "dave: {:?}", dave.seen);
        for p in mesh.publishers.iter_mut() {
            p.pump();
            p.send_due();
        }
        mesh.subscriptions.iter_mut().for_each(|(_, _, p)| p.pump());
        dave.pump();
        if dave.connected() {
            connected_at.get_or_insert_with(Instant::now);
        }
        if mesh.publishers[0].seen.keyframe_requests > asked_before {
            asked_at.get_or_insert_with(Instant::now);
        }
        std::thread::sleep(Duration::from_millis(3));
    }
    let (connected_at, asked_at) = (connected_at.unwrap(), asked_at.expect("alice was asked for a keyframe"));
    let delay = asked_at.saturating_duration_since(connected_at);
    eprintln!("dave connected after {:?}; alice asked {delay:?} after that", connected_at - start);
    assert!(delay < Duration::from_millis(500), "the keyframe request took {delay:?}");
    assert!(dave.seen.keyframes >= 1);
    let (_, body) = http(&server, "GET", &format!("/rooms/{}", mesh.room), "");
    assert!(participant(&body, "alice").unwrap().contains("\"subscribers\":[\"bob\",\"carol\",\"dave\"]"), "{body}");
    assert!(participant(&body, "dave").unwrap().contains("\"publishing\":false,\"connected\":true"), "{body}");
}

#[test]
fn deleting_a_participant_closes_its_connections_and_no_other() {
    let (server, _log) = server(|_| {});
    let names = ["alice", "bob", "carol"];
    let mut mesh = Mesh::new(&server, &names);
    mesh.run(Duration::from_millis(500));

    let (status, body) = http(&server, "DELETE", &format!("/rooms/{}/participants/bob", mesh.room), "");
    assert_eq!(status, 204, "{body}");
    let bobs = |i: usize, j: usize| i == 1 || j == 1;
    until("bob's connections and everyone's subscriptions of bob close", Duration::from_secs(5), || {
        mesh.pump();
        mesh.publishers[1].state() == Some(State::Closed)
            && mesh.subscriptions.iter().filter(|(i, j, _)| bobs(*i, *j)).all(|(_, _, p)| p.state() == Some(State::Closed))
    });
    // The rest goes on.
    let frames_before = mesh.subscription(0, 2).seen.frames;
    mesh.run(Duration::from_millis(1000));
    assert_eq!(mesh.publishers[0].state(), Some(State::Connected));
    assert_eq!(mesh.publishers[2].state(), Some(State::Connected));
    for (i, j, phone) in &mesh.subscriptions {
        if !bobs(*i, *j) {
            assert_eq!(phone.state(), Some(State::Connected), "{} <- {}", names[*i], names[*j]);
            assert!(!phone.seen.states.contains(&State::Closed));
        }
    }
    assert!(mesh.subscription(0, 2).seen.frames > frames_before + 20, "alice still gets carol");

    let (_, body) = http(&server, "GET", &format!("/rooms/{}", mesh.room), "");
    assert!(participant(&body, "bob").is_none(), "{body}");
    assert!(participant(&body, "alice").unwrap().contains("\"subscribers\":[\"carol\"]"), "{body}");
    assert!(participant(&body, "carol").unwrap().contains("\"subscribers\":[\"alice\"]"), "{body}");
    let (status, _) = http(&server, "DELETE", &format!("/rooms/{}/participants/bob", mesh.room), "");
    assert_eq!(status, 404, "bob is gone");
    let (status, _) = http(&server, "POST", &format!("/rooms/{}/subscribe/alice/bob", mesh.room), &Phone::new("x", false).conn.offer(false).unwrap());
    assert_eq!(status, 404, "bob publishes nothing any more");
}

#[test]
fn an_idle_publisher_is_dropped_and_its_subscribers_see_it_close() {
    let (server, log) = server(|c| c.connection_idle = Duration::from_millis(1500));
    let room = create_room(&server);
    let nat = Proxy::new(server.udp_addr());
    let mut alice = Phone::new("alice", false);
    let mut bob = Phone::new("bob", false);
    join(&server, &alice, &format!("/rooms/{room}/publish/alice"), Some(&nat));
    join(&server, &bob, &format!("/rooms/{room}/publish/bob"), None);
    let mut of_alice = Phone::new("carol<-alice", false);
    let mut of_bob = Phone::new("carol<-bob", false);
    join(&server, &of_alice, &format!("/rooms/{room}/subscribe/carol/alice"), None);
    join(&server, &of_bob, &format!("/rooms/{room}/subscribe/carol/bob"), None);
    until("connected", Duration::from_secs(10), || {
        [&mut alice, &mut bob, &mut of_alice, &mut of_bob].iter_mut().for_each(|p| p.pump());
        [&alice, &bob, &of_alice, &of_bob].iter().all(|p| p.connected())
    });
    run_for(Duration::from_millis(500), &mut [&mut alice, &mut bob], &mut [&mut of_alice, &mut of_bob]);
    assert!(of_alice.seen.frames > 5 && of_bob.seen.frames > 5);

    // Alice's network goes away: no goodbye, only silence.
    nat.cut(true);
    let cut_at = Instant::now();
    let start = Instant::now();
    while of_alice.state() != Some(State::Closed) {
        assert!(start.elapsed() < Duration::from_secs(5), "carol<-alice: {:?}", of_alice.seen.states);
        run_for(Duration::from_millis(20), &mut [&mut alice, &mut bob], &mut [&mut of_alice, &mut of_bob]);
    }
    let closed_after = cut_at.elapsed();
    eprintln!("carol's subscription to alice closed {closed_after:?} after alice went silent");
    assert!(closed_after >= Duration::from_millis(1400), "closed before the idle timeout: {closed_after:?}");
    run_for(Duration::from_millis(500), &mut [&mut alice, &mut bob], &mut [&mut of_alice, &mut of_bob]);
    assert_eq!(of_bob.state(), Some(State::Connected), "carol<-bob goes on");
    assert_eq!(bob.state(), Some(State::Connected));

    let mut seen = Vec::new();
    lines(&log, &mut seen);
    let said = |what: &str| seen.iter().any(|l| l.contains(what));
    assert!(said("alice's publish") && said("closed: idle"), "{seen:#?}");
    assert!(said("carol's subscription to alice") && said("closed: alice left"), "{seen:#?}");
    let (_, body) = http(&server, "GET", &format!("/rooms/{room}"), "");
    assert!(participant(&body, "alice").is_none(), "{body}");
    assert!(participant(&body, "bob").unwrap().contains("\"subscribers\":[\"carol\"]"), "{body}");
}

#[test]
fn publishing_again_keeps_the_subscribers() {
    let (server, _log) = server(|_| {});
    let room = create_room(&server);
    let mut first = Phone::new("alice-1", false);
    let mut sub = Phone::new("bob<-alice", false);
    join(&server, &first, &format!("/rooms/{room}/publish/alice"), None);
    join(&server, &sub, &format!("/rooms/{room}/subscribe/bob/alice"), None);
    until("connected", Duration::from_secs(10), || {
        first.pump();
        sub.pump();
        first.connected() && sub.connected()
    });
    run_for(Duration::from_millis(700), &mut [&mut first], &mut [&mut sub]);
    let before = sub.seen.frames;
    assert!(before > 10);

    // Alice publishes again (another connection: a phone that changed networks, say).
    let mut second = Phone::new("alice-2", false);
    join(&server, &second, &format!("/rooms/{room}/publish/alice"), None);
    until("the first publish closes, the second connects", Duration::from_secs(10), || {
        first.pump();
        second.pump();
        sub.pump();
        first.state() == Some(State::Closed) && second.connected()
    });
    let keyframes_before = sub.seen.keyframes;
    run_for(Duration::from_millis(1000), &mut [&mut second], &mut [&mut sub]);
    assert_eq!(sub.state(), Some(State::Connected), "the subscription stays: {:?}", sub.seen.states);
    assert!(sub.seen.frames > before + 20, "frames from the new publish: {} -> {}", before, sub.seen.frames);
    assert!(sub.seen.keyframes > keyframes_before, "it starts from a keyframe");
    let (_, body) = http(&server, "GET", &format!("/rooms/{room}"), "");
    assert!(participant(&body, "alice").unwrap().contains("\"publishing\":true,\"connected\":true,\"subscribers\":[\"bob\"]"), "{body}");
}

#[test]
fn a_publisher_whose_nat_moves_is_followed() {
    let (server, log) = server(|_| {});
    let room = create_room(&server);
    let nat = Proxy::new(server.udp_addr());
    let mut alice = Phone::new("alice", false);
    let mut sub = Phone::new("bob<-alice", false);
    join(&server, &alice, &format!("/rooms/{room}/publish/alice"), Some(&nat));
    join(&server, &sub, &format!("/rooms/{room}/subscribe/bob/alice"), None);
    until("connected", Duration::from_secs(10), || {
        alice.pump();
        sub.pump();
        alice.connected() && sub.connected()
    });
    run_for(Duration::from_millis(500), &mut [&mut alice], &mut [&mut sub]);
    let before = sub.seen.frames;
    let old = nat.outward_addr();
    let new = nat.rebind();
    assert_ne!(old, new);
    let moved_at = Instant::now();
    let start = Instant::now();
    while sub.seen.frames < before + 30 {
        assert!(start.elapsed() < Duration::from_secs(10), "no frames after the NAT moved: {} -> {}", before, sub.seen.frames);
        run_for(Duration::from_millis(20), &mut [&mut alice], &mut [&mut sub]);
    }
    eprintln!("frames flow again {:?} after the NAT moved", moved_at.elapsed());
    assert_eq!(alice.state(), Some(State::Connected));
    let mut seen = Vec::new();
    lines(&log, &mut seen);
    assert!(!seen.iter().any(|l| l.contains("closed")), "{seen:#?}");
}
