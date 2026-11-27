//! Both modes at once, on the one UDP port: a room the server is a peer of, and a call it only
//! relays, side by side.

mod common;

use std::time::Duration;

use common::*;

#[test]
fn a_room_and_a_call_share_the_port() {
    let (server, _log) = server(|_| {});
    let room = create_room(&server);
    let (mut alice, mut of_alice) = (Phone::new("alice", false), Phone::new("bob<-alice", false));
    join(&server, &alice, &format!("/rooms/{room}/publish/alice"), None);
    join(&server, &of_alice, &format!("/rooms/{room}/subscribe/bob/alice"), None);

    let (mut c, mut d) = (Phone::new("c", true), Phone::new("d", true));
    let offer = c.conn.offer(false).unwrap();
    let answer = d.conn.accept_offer(&offer).unwrap();
    c.conn.accept_answer(&answer).unwrap();
    let body = format!("{{\"ufrags\":[\"{}\",\"{}\"]}}", ufrag_of(&offer), ufrag_of(&answer));
    let (status, body) = http(&server, "POST", "/calls", &body);
    assert_eq!(status, 201, "{body}");
    let (call, candidate) = (field(&body, "id").unwrap().to_owned(), field(&body, "candidate").unwrap().to_owned());
    c.conn.add_candidate(&candidate);
    d.conn.add_candidate(&candidate);

    until("all four connected", Duration::from_secs(10), || {
        [&mut alice, &mut of_alice, &mut c, &mut d].iter_mut().for_each(|p| p.pump());
        [&alice, &of_alice, &c, &d].iter().all(|p| p.connected())
    });
    run_for(Duration::from_secs(2), &mut [&mut alice, &mut c, &mut d], &mut [&mut of_alice]);
    std::thread::sleep(Duration::from_millis(300));
    [&mut alice, &mut of_alice, &mut c, &mut d].iter_mut().for_each(|p| p.pump());

    eprintln!(
        "room: {} of {} frames; call: c {} of {}, d {} of {}",
        of_alice.seen.frames, alice.sent_frames(), c.seen.frames, d.sent_frames(), d.seen.frames, c.sent_frames()
    );
    assert!(of_alice.seen.frames + 5 >= alice.sent_frames() && of_alice.seen.audio + 10 >= alice.sent_audio());
    assert!(c.seen.frames + 3 >= d.sent_frames() && d.seen.frames + 3 >= c.sent_frames());

    let (_, room_body) = http(&server, "GET", &format!("/rooms/{room}"), "");
    assert!(participant(&room_body, "alice").unwrap().contains("\"publishing\":true,\"connected\":true,\"subscribers\":[\"bob\"]"), "{room_body}");
    assert!(participant(&room_body, "bob").unwrap().contains("\"publishing\":false,\"connected\":true,\"subscribers\":[]"), "{room_body}");
    let (_, call_body) = http(&server, "GET", &format!("/calls/{call}"), "");
    assert!(!call_body.contains("\"connected\":false") && call_body.contains("\"connected\":true"), "{call_body}");
}
