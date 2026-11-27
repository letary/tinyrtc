//! `h264` end to end, through the real thing: access units framed as the platforms frame them go
//! through `h264::to_packetizer`, str0m's packetizer (STAP-A, FU-A), an in-memory wire, str0m's
//! depacketizer, and `h264::nals` on the far side, which must give back the same NAL units byte
//! for byte. It also pins the str0m facts the module is built on (src/h264.rs, the header): the
//! depacketized sample is Annex B with `00 00 00 01` before every unit and nothing else, the SPS
//! and the PPS come out as two units, an AUD is dropped, an SEI is kept. Red here after a str0m
//! bump means one of those moved.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tinyrtc::h264::{self, Framing};
use tinyrtc::profile::config;
use str0m::change::SdpOffer;
use str0m::format::Codec;
use str0m::media::{Direction, MediaKind, MediaTime};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc};

/// One depacketized H.264 sample, as the receiving side saw it.
struct Received {
    data: Vec<u8>,
    keyframe: bool,
    /// RTP packets it came in.
    packets: u64,
}

struct Peer {
    rtc: Rtc,
    addr: SocketAddr,
    out: VecDeque<Vec<u8>>,
    deadline: Instant,
    connected: bool,
    received: Vec<Received>,
}

impl Peer {
    fn new(addr: &str, now: Instant) -> Peer {
        let addr = addr.parse().unwrap();
        let mut rtc = config().build(now);
        rtc.add_local_candidate(Candidate::host(addr, "udp").expect("candidate"));
        let (out, received) = (VecDeque::new(), Vec::new());
        Peer { rtc, addr, out, deadline: now, connected: false, received }
    }

    /// After every mutation, poll until str0m hands back a timeout. An `Err` here would be a
    /// sample str0m could not depacketize: a failure of this test, not something to step over.
    fn drain(&mut self) {
        loop {
            match self.rtc.poll_output().expect("poll_output") {
                Output::Timeout(t) => {
                    self.deadline = t;
                    return;
                }
                Output::Transmit(t) => self.out.push_back(t.contents.to_vec()),
                Output::Event(Event::Connected) => self.connected = true,
                Output::Event(Event::MediaData(d)) if d.params.spec().codec == Codec::H264 => {
                    self.received.push(Received {
                        data: d.data.to_vec(),
                        keyframe: d.is_keyframe(),
                        packets: **d.seq_range.end() - **d.seq_range.start() + 1,
                    });
                }
                Output::Event(_) => {}
            }
        }
    }

    fn timeout(&mut self, now: Instant) {
        self.rtc.handle_input(Input::Timeout(now)).expect("timeout");
        self.drain();
    }

    fn receive(&mut self, now: Instant, source: SocketAddr, bytes: &[u8]) {
        let contents = bytes.try_into().expect("datagram");
        let receive = Receive { proto: Protocol::Udp, source, destination: self.addr, contents };
        self.rtc.handle_input(Input::Receive(now, receive)).expect("receive");
        self.drain();
    }
}

/// Every queued datagram to the other side, until both are quiet.
fn shuttle(a: &mut Peer, b: &mut Peer, now: Instant) {
    loop {
        let mut moved = false;
        while let Some(bytes) = a.out.pop_front() {
            b.receive(now, a.addr, &bytes);
            moved = true;
        }
        while let Some(bytes) = b.out.pop_front() {
            a.receive(now, b.addr, &bytes);
            moved = true;
        }
        if !moved {
            return;
        }
    }
}

/// A NAL unit: `header`, then `len` bytes of payload with plenty of zeros, emulation prevention
/// applied as an encoder applies it (no `00 00 0x`, x <= 3, left: H.264 7.4.1), and a stop bit
/// at the end, so it never ends in 0x00.
fn unit(header: u8, len: usize, seed: u32) -> Vec<u8> {
    let mut out = vec![header];
    let (mut x, mut zeros) = (seed.wrapping_mul(2_654_435_761) | 1, 0);
    for _ in 0..len {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        let b = if x % 5 == 0 { 0 } else { (x >> 8) as u8 };
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out.push(0x80);
    out
}

fn avcc(units: &[&Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for u in units {
        out.extend_from_slice(&(u.len() as u32).to_be_bytes());
        out.extend_from_slice(u);
    }
    out
}

/// What str0m hands the receiver for these units (module header, (b)).
fn depacketized(units: &[Vec<u8>]) -> Vec<u8> {
    units.iter().flat_map(|u| [0, 0, 0, 1].iter().chain(u)).copied().collect()
}

struct Frame {
    name: &'static str,
    framing: Framing,
    bytes: Vec<u8>,
    /// The NAL units that must arrive.
    expect: Vec<Vec<u8>>,
    keyframe: bool,
}

fn frames() -> Vec<Frame> {
    // An SPS with emulation prevention in it; a PPS; an IDR far over one packet (FU-A).
    let sps = vec![0x67, 0x42, 0xe0, 0x1f, 0x8d, 0x68, 0x05, 0x00, 0x00, 0x03, 0x00, 0x01, 0x80];
    let pps = vec![0x68, 0xce, 0x3c, 0x80];
    let idr = unit(0x65, 20_000, 1);
    let small_p = unit(0x41, 600, 2);
    let aud = vec![0x09, 0xf0];
    let sei = unit(0x06, 30, 3);
    let p = unit(0x41, 2_500, 4);
    let idr2 = unit(0x25, 5_000, 5);

    // MediaCodec's way, untidy: leading zeros, an AUD, 3- and 4-byte start codes, trailing zeros.
    let mut annex_b_p = vec![0, 0, 0, 0, 1];
    annex_b_p.extend_from_slice(&aud);
    annex_b_p.extend_from_slice(&[0, 0, 1]);
    annex_b_p.extend_from_slice(&sei);
    annex_b_p.extend_from_slice(&[0, 0, 0, 0, 0, 1]);
    annex_b_p.extend_from_slice(&p);
    annex_b_p.extend_from_slice(&[0, 0]);
    let mut annex_b_key = Vec::new();
    for u in [&sps, &pps, &idr2] {
        annex_b_key.extend_from_slice(&[0, 0, 1]);
        annex_b_key.extend_from_slice(u);
    }

    vec![
        Frame {
            name: "AVCC keyframe",
            framing: Framing::Avcc,
            bytes: avcc(&[&sps, &pps, &idr]),
            expect: vec![sps.clone(), pps.clone(), idr],
            keyframe: true,
        },
        Frame {
            name: "AVCC P frame",
            framing: Framing::Avcc,
            bytes: avcc(&[&small_p]),
            expect: vec![small_p],
            keyframe: false,
        },
        Frame {
            name: "Annex B P frame with AUD and SEI",
            framing: Framing::AnnexB,
            bytes: annex_b_p,
            expect: vec![sei, p], // str0m drops the AUD (src/packet/h264.rs:136)
            keyframe: false,
        },
        Frame {
            name: "Annex B keyframe",
            framing: Framing::AnnexB,
            bytes: annex_b_key,
            expect: vec![sps, pps, idr2],
            keyframe: true,
        },
    ]
}

#[test]
fn h264_access_units_survive_str0m() {
    let frames = frames();
    let start = Instant::now();
    let mut tx = Peer::new("10.0.0.1:5000", start);
    let mut rx = Peer::new("10.0.0.2:5000", start);

    let mut change = tx.rtc.sdp_api();
    let mid = change.add_media(MediaKind::Video, Direction::SendRecv, None, None, None);
    let (offer, pending) = change.apply().expect("offer");
    let offer = SdpOffer::from_sdp_string(&offer.to_sdp_string()).expect("offer parses");
    let answer = rx.rtc.sdp_api().accept_offer(offer).expect("answer");
    tx.rtc.sdp_api().accept_answer(pending, answer).expect("accept answer");
    tx.drain();
    rx.drain();
    let pt = tx.rtc.codec_config().find(|p| p.spec().codec == Codec::H264).expect("H.264").pt();

    let mut scratch = Vec::new();
    let (mut sent, mut last_sent) = (0, None);
    let mut now = start;
    while rx.received.len() < frames.len() {
        let (arrived, all) = (rx.received.len(), frames.len());
        assert!(now - start < Duration::from_secs(10), "{arrived} of {all} frames arrived");
        now += Duration::from_millis(1);
        if tx.deadline <= now {
            tx.timeout(now);
        }
        if rx.deadline <= now {
            rx.timeout(now);
        }
        shuttle(&mut tx, &mut rx, now);

        let due = last_sent.is_none_or(|t| now - t >= Duration::from_millis(33));
        if tx.connected && rx.connected && sent < frames.len() && due {
            let f = &frames[sent];
            assert_eq!(h264::is_keyframe(&f.bytes, f.framing), f.keyframe, "{}", f.name);
            assert_eq!(h264::has_parameter_sets(&f.bytes, f.framing), f.keyframe, "{}", f.name);
            let clean = h264::to_packetizer(&f.bytes, f.framing, &mut scratch);
            assert!(clean, "{}: malformed", f.name);
            let time = MediaTime::from_90khz(3_000 * (sent as u64 + 1));
            let writer = tx.rtc.writer(mid).expect("writer");
            writer.write(pt, now, time, scratch.as_slice()).expect("write");
            tx.drain();
            shuttle(&mut tx, &mut rx, now);
            sent += 1;
            last_sent = Some(now);
        }
    }

    for (f, r) in frames.iter().zip(&rx.received) {
        let (sent, got) = (f.bytes.len(), r.data.len());
        println!("{}: {sent} bytes in, {got} bytes out, {} packets", f.name, r.packets);
        let mut walk = h264::nals(&r.data, Framing::AnnexB);
        let got: Vec<Vec<u8>> = (&mut walk).map(|n| n.data.to_vec()).collect();
        assert!(!walk.malformed(), "{}", f.name);
        assert_eq!(got.len(), f.expect.len(), "{}: unit count", f.name);
        assert!(got == f.expect, "{}: the units differ", f.name);
        assert!(r.data == depacketized(&f.expect), "{}: not 00 00 00 01 + unit, each", f.name);
        assert_eq!(r.keyframe, f.keyframe, "{}: str0m's is_keyframe", f.name);
        assert_eq!(h264::is_keyframe(&r.data, Framing::AnnexB), f.keyframe, "{}", f.name);
        assert_eq!(h264::has_parameter_sets(&r.data, Framing::AnnexB), f.keyframe, "{}", f.name);
    }
    // The keyframes went as STAP-A + FU-A, the small P frame as one single-NAL packet.
    assert!(rx.received[0].packets >= 3, "AVCC keyframe in {} packets", rx.received[0].packets);
    assert_eq!(rx.received[1].packets, 1, "small P frame");
    assert!(rx.received[3].packets >= 3, "Annex B keyframe in {} packets", rx.received[3].packets);
}
