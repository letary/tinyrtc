//! A load measurement, not a check: the release binary as its own process, rooms and calls of
//! in-process phones sending ~1 Mbit/s of video each, and the server process's CPU time over a
//! steady window. Prints the numbers.
//!
//!   cargo test --release --test load -- --ignored --nocapture
//!   LOAD_ROOMS=10 LOAD_SUBSCRIBERS=3 LOAD_CALLS=5 LOAD_SECONDS=10 (the defaults)

mod common;

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use tinyrtc::connection::{AudioOut, VideoOut};

fn env(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// ~1 Mbit/s at 30 fps: 4 kB frames, a 20 kB keyframe every 2 s.
fn frame(key: bool, n: u32) -> Arc<[u8]> {
    let mut au = vec![0, 0, 0, 1, if key { 0x65 } else { 0x41 }];
    au.extend((0..if key { 20_000 } else { 4_000 }).map(|i: u32| (i.wrapping_mul(31).wrapping_add(n) % 251) as u8 | 0x10));
    au.into()
}

fn cpu_ms(pid: u32) -> f64 {
    if cfg!(windows) {
        let out = Command::new("powershell")
            .args(["-NoProfile", "-Command", &format!("(Get-Process -Id {pid}).TotalProcessorTime.TotalMilliseconds")])
            .output()
            .expect("powershell");
        String::from_utf8_lossy(&out.stdout).trim().replace(',', ".").parse().expect("a number")
    } else {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("/proc");
        let fields: Vec<&str> = stat.rsplit(')').next().unwrap().split_whitespace().collect();
        let ticks: f64 = fields[11].parse::<f64>().unwrap() + fields[12].parse::<f64>().unwrap();
        ticks * 10.0 // USER_HZ = 100
    }
}

struct Binary(Child);

impl Drop for Binary {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

#[test]
#[ignore = "a measurement: run it on purpose"]
fn load() {
    let (rooms, subscribers, calls, seconds) =
        (env("LOAD_ROOMS", 10), env("LOAD_SUBSCRIBERS", 3), env("LOAD_CALLS", 5), env("LOAD_SECONDS", 10));
    let mut child = Command::new(env!("CARGO_BIN_EXE_tinyrtc-mediaserver"))
        .args(["--public-ip", "127.0.0.1", "--udp", "127.0.0.1:0", "--http", "127.0.0.1:0", "--secret", SECRET])
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary");
    let pid = child.id();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let server = Binary(child);
    let mut line = String::new();
    stderr.read_line(&mut line).unwrap();
    // "<time> started: udp A (candidate B), http C"
    let word = |after: &str| -> SocketAddr { line.split(after).nth(1).unwrap().split([' ', ',', ')']).next().unwrap().trim().parse().unwrap() };
    let (udp, api) = (word("udp "), word("http "));
    std::thread::spawn(move || for _ in stderr.lines() {});
    let request = |method: &str, path: &str, body: &str| http_raw(api, method, path, Some(SECRET), body);

    // Rooms: one publisher and `subscribers` subscribers each.
    let mut publishers = Vec::new();
    let mut others = Vec::new();
    for r in 0..rooms {
        let (_, body) = request("POST", "/rooms", "");
        let room = field(&body, "id").unwrap().to_owned();
        let publisher = Phone::new(&format!("p{r}"), false);
        let (status, answer) = request("POST", &format!("/rooms/{room}/publish/p"), &publisher.conn.offer(false).unwrap());
        assert_eq!(status, 200, "{answer}");
        publisher.conn.accept_answer(&answer).unwrap();
        for s in 0..subscribers {
            let phone = Phone::new(&format!("s{r}.{s}"), false);
            let (status, answer) = request("POST", &format!("/rooms/{room}/subscribe/s{s}/p"), &phone.conn.offer(false).unwrap());
            assert_eq!(status, 200, "{answer}");
            phone.conn.accept_answer(&answer).unwrap();
            others.push(phone);
        }
        publishers.push(publisher);
    }
    // Calls: both phones send.
    // POST /calls's line (the phones are relay only: they take no other kind)
    let candidate = format!("candidate:1 1 udp 255 {} {} typ relay raddr 0.0.0.0 rport 0", udp.ip(), udp.port());
    for c in 0..calls {
        let (a, b) = (Phone::new(&format!("c{c}a"), true), Phone::new(&format!("c{c}b"), true));
        let offer = a.conn.offer(false).unwrap();
        let answer = b.conn.accept_offer(&offer).unwrap();
        a.conn.accept_answer(&answer).unwrap();
        let (status, _) = request("POST", "/calls", &format!("{{\"ufrags\":[\"{}\",\"{}\"]}}", ufrag_of(&offer), ufrag_of(&answer)));
        assert_eq!(status, 201);
        a.conn.add_candidate(&candidate);
        b.conn.add_candidate(&candidate);
        publishers.push(a);
        publishers.push(b);
    }
    until("everyone connected", Duration::from_secs(30), || {
        publishers.iter_mut().chain(others.iter_mut()).for_each(Phone::pump);
        publishers.iter().chain(others.iter()).all(Phone::connected)
    });

    // An encoder answers a keyframe request with its next frame (as a phone's does).
    let mut answered = vec![0u32; publishers.len()];
    let mut send = |publishers: &mut Vec<Phone>, n: u32| {
        let now = Instant::now();
        for (p, answered) in publishers.iter().zip(answered.iter_mut()) {
            let key = n.is_multiple_of(60) || p.seen.keyframe_requests > *answered;
            *answered = p.seen.keyframe_requests;
            p.conn.send_video(VideoOut { data: frame(key, n), captured: now, rotation: 0 });
            if n % 3 != 2 {
                // ~50 Opus packets a second next to 30 frames.
                let data: Arc<[u8]> = vec![0x78; 80].into();
                p.conn.send_audio(AudioOut { data, time: u64::from(n) * 960, captured: now });
            }
        }
    };
    let warmup = Instant::now();
    let mut n = 0u32;
    while warmup.elapsed() < Duration::from_secs(2) {
        send(&mut publishers, n);
        n += 1;
        std::thread::sleep(Duration::from_millis(33));
        publishers.iter_mut().chain(others.iter_mut()).for_each(Phone::pump);
    }
    let frames_before: u32 = others.iter().map(|p| p.seen.frames).sum();
    let (cpu0, t0, n0) = (cpu_ms(pid), Instant::now(), n);
    let mut next = Instant::now();
    while t0.elapsed() < Duration::from_secs(seconds as u64) {
        send(&mut publishers, n);
        n += 1;
        next += Duration::from_millis(33);
        publishers.iter_mut().chain(others.iter_mut()).for_each(Phone::pump);
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    let (cpu1, elapsed) = (cpu_ms(pid), t0.elapsed().as_secs_f64());
    let sent_per_sub = n - n0;
    // What is still queued in a pacer arrives late; the frames before the window arrive early.
    std::thread::sleep(Duration::from_millis(1500));
    publishers.iter_mut().chain(others.iter_mut()).for_each(Phone::pump);
    let _ = frames_before;
    // Over the whole run: every stream was sent `n` frames.
    let received: Vec<u32> = others.iter().chain(&publishers[rooms..]).map(|p| p.seen.frames).collect();
    let least = received.iter().min().copied().unwrap_or(0);
    let average = received.iter().sum::<u32>() / received.len().max(1) as u32;
    let cpu = (cpu1 - cpu0) / 1000.0 / elapsed;
    let streams_out = rooms * subscribers;
    println!(
        "load: {rooms} rooms x (1 publisher + {subscribers} subscribers) + {calls} calls, {elapsed:.1} s steady \
         ({sent_per_sub} frames a stream): server CPU {:.1} % of one core; over the whole run {n} frames a stream \
         were sent, {average} received on average, {least} at least; ~{:.0} Mbit/s into the server, ~{:.0} Mbit/s out",
        cpu * 100.0,
        (rooms + 2 * calls) as f64 * 1.1,
        (streams_out + 2 * calls) as f64 * 1.1,
    );
    drop(server);
}
