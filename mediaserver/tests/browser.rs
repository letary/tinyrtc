//! A real browser in a room: Chrome publishes (its own H.264 and Opus), Chrome subscribes through
//! the server, and a phone of the library subscribes to the same publisher. tests/browser/rooms.mjs
//! drives the two pages and plays the backend.
//!
//!   cargo test --release --test browser -- --ignored --nocapture
//!
//! Needs node, Chrome, and Playwright (rooms.mjs says where it looks).

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::*;

#[test]
#[ignore = "needs node, Playwright and Chrome"]
fn chrome_publishes_and_subscribes_through_the_server() {
    let (server, _log) = server(|_| {});
    let room = create_room(&server);
    let mut node = Command::new("node")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["tests/browser/rooms.mjs", "--http", &server.http_addr().to_string(), "--secret", SECRET, "--room", &room])
        .args(["--seconds", "8"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("node");
    let stdout = node.stdout.take().unwrap();
    let printer = std::thread::spawn(move || {
        let mut last = String::new();
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            println!("[node] {line}");
            last = line;
        }
        last
    });

    // Once Chrome publishes, a phone of the library subscribes to it too.
    let start = Instant::now();
    loop {
        assert!(start.elapsed() < Duration::from_secs(30), "Chrome never published");
        let (_, body) = http(&server, "GET", &format!("/rooms/{room}"), "");
        if participant(&body, "alice").is_some_and(|p| p.contains("\"publishing\":true,\"connected\":true")) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut phone = Phone::new("carol<-alice", false);
    join(&server, &phone, &format!("/rooms/{room}/subscribe/carol/alice"), None);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        phone.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    println!("the library's phone, from Chrome: {:?}", phone.seen);

    let status = node.wait().unwrap();
    let last = printer.join().unwrap();
    assert!(status.success(), "the browser checks failed: {last}");
    assert!(phone.connected(), "{:?}", phone.seen.states);
    assert!(phone.seen.frames > 50 && phone.seen.keyframes >= 1, "{:?}", phone.seen);
    assert!(phone.seen.audio > 100, "{:?}", phone.seen);
}
