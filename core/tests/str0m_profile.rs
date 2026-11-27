//! The ground the library stands on, checked without any of its own code: two str0m instances in
//! one process, joined by an in-memory wire and driven in virtual time, run the whole profile —
//! one H.264 line, one Opus line, a data channel, bandwidth estimation. Red here after a str0m bump
//! means the bump broke the profile, not us.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use str0m::bwe::{Bitrate, BweKind};
use str0m::change::SdpOffer;
use str0m::crypto::CryptoProvider;
use str0m::channel::ChannelId;
use str0m::format::Codec;
use str0m::media::{Direction, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use tinyrtc::profile::config;
use str0m::{Candidate, Event, Input, Output, Rtc};

/// What one side saw: the numbers the spike is judged by.
#[derive(Debug, Default, Clone)]
pub struct Seen {
    pub connected: bool,
    pub video_frames: u32,
    pub video_bytes: u64,
    pub video_keyframes: u32,
    pub audio_frames: u32,
    pub channel_messages: u32,
    pub channel_open: bool,
    pub keyframe_requests: u32,
    pub bwe_estimates: u32,
    pub last_bwe_bps: u64,
}

struct Peer {
    rtc: Rtc,
    addr: SocketAddr,
    /// Datagrams this side wants on the wire: (destination, bytes).
    out: VecDeque<(SocketAddr, Vec<u8>)>,
    deadline: Instant,
    seen: Seen,
    video: Option<Mid>,
    audio: Option<Mid>,
    channel: Option<ChannelId>,
}

impl Peer {
    fn new(addr: SocketAddr, now: Instant, crypto: Option<Arc<CryptoProvider>>) -> Peer {
        let config = match crypto {
            Some(provider) => config().set_crypto_provider(provider),
            None => config(),
        };
        let mut rtc = config.build(now);
        rtc.add_local_candidate(Candidate::host(addr, "udp").expect("candidate"));
        Peer {
            rtc,
            addr,
            out: VecDeque::new(),
            deadline: now,
            seen: Seen::default(),
            video: None,
            audio: None,
            channel: None,
        }
    }

    /// The rule of str0m: after every mutation, poll until it hands back a timeout.
    fn drain(&mut self) {
        loop {
            match self.rtc.poll_output().expect("poll_output") {
                Output::Timeout(t) => {
                    self.deadline = t;
                    return;
                }
                Output::Transmit(t) => self.out.push_back((t.destination, t.contents.to_vec())),
                Output::Event(e) => self.on_event(e),
            }
        }
    }

    fn on_event(&mut self, e: Event) {
        match e {
            Event::Connected => self.seen.connected = true,
            Event::MediaAdded(m) => match m.kind {
                MediaKind::Video => self.video = Some(m.mid),
                MediaKind::Audio => self.audio = Some(m.mid),
            },
            Event::MediaData(d) => {
                if d.params.spec().codec == Codec::H264 {
                    self.seen.video_frames += 1;
                    self.seen.video_bytes += d.data.len() as u64;
                    if d.is_keyframe() {
                        self.seen.video_keyframes += 1;
                    }
                } else {
                    self.seen.audio_frames += 1;
                }
            }
            Event::ChannelOpen(id, _) => {
                self.channel = Some(id);
                self.seen.channel_open = true;
            }
            Event::ChannelData(_) => self.seen.channel_messages += 1,
            Event::KeyframeRequest(_) => self.seen.keyframe_requests += 1,
            Event::EgressBitrateEstimate(BweKind::Twcc { estimate, .. }) => {
                self.seen.bwe_estimates += 1;
                self.seen.last_bwe_bps = estimate.as_u64();
            }
            _ => {}
        }
    }

    fn timeout(&mut self, now: Instant) {
        self.rtc.handle_input(Input::Timeout(now)).expect("timeout");
        self.drain();
    }

    fn receive(&mut self, now: Instant, source: SocketAddr, bytes: &[u8]) {
        let Ok(contents) = bytes.try_into() else { return };
        let input = Input::Receive(
            now,
            Receive { proto: Protocol::Udp, source, destination: self.addr, contents },
        );
        self.rtc.handle_input(input).expect("receive");
        self.drain();
    }

    fn pt(&self, codec: Codec) -> str0m::media::Pt {
        self.rtc
            .codec_config()
            .find(|p| p.spec().codec == codec)
            .expect("codec negotiated")
            .pt()
    }

    fn send(&mut self, mid: Mid, codec: Codec, now: Instant, time: MediaTime, data: Vec<u8>) {
        let pt = self.pt(codec);
        if let Some(w) = self.rtc.writer(mid) {
            w.write(pt, now, time, data).expect("write");
        }
        self.drain();
    }
}

/// A fake Annex B access unit: the NAL headers are real (SPS / PPS / IDR, or one non-IDR slice),
/// the payload is filler — the packetizer and the depacketizer read nothing below the header.
fn fake_h264(key: bool, n: u32) -> Vec<u8> {
    let mut au = Vec::new();
    let mut nal = |header: u8, len: usize| {
        au.extend_from_slice(&[0, 0, 0, 1, header]);
        // No 00 00 0x run inside: it would read as a start code.
        au.extend((0..len).map(|i| ((i as u32).wrapping_mul(31).wrapping_add(n) % 251) as u8 | 0x10));
    };
    if key {
        nal(0x67, 24);
        nal(0x68, 6);
        nal(0x65, 20_000);
    } else {
        nal(0x41, 3_000);
    }
    au
}

/// Move every queued datagram to the other side, until both are quiet.
fn shuttle(a: &mut Peer, b: &mut Peer, now: Instant) {
    loop {
        let mut moved = false;
        while let Some((_, bytes)) = a.out.pop_front() {
            b.receive(now, a.addr, &bytes);
            moved = true;
        }
        while let Some((_, bytes)) = b.out.pop_front() {
            a.receive(now, b.addr, &bytes);
            moved = true;
        }
        if !moved {
            return;
        }
    }
}

/// Runs `seconds` of a call in virtual time (no sleeping) and returns what each side saw.
pub fn loopback(seconds: u64) -> (Seen, Seen) {
    loopback_with(seconds, None, None)
}

/// The same call with a chosen crypto backend on each side (None = the one the features select).
pub fn loopback_with(
    seconds: u64,
    left: Option<Arc<CryptoProvider>>,
    right: Option<Arc<CryptoProvider>>,
) -> (Seen, Seen) {
    let start = Instant::now();
    let mut l = Peer::new("10.0.0.1:5000".parse().unwrap(), start, left);
    let mut r = Peer::new("10.0.0.2:5000".parse().unwrap(), start, right);

    // The offer: video + audio both ways, and a data channel.
    let mut change = l.rtc.sdp_api();
    let video = change.add_media(MediaKind::Video, Direction::SendRecv, None, None, None);
    let audio = change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
    let channel = change.add_channel("data".into());
    let (offer, pending) = change.apply().expect("offer");
    l.video = Some(video);
    l.audio = Some(audio);
    l.channel = Some(channel);

    // Through text, as it will cross the app's signaling.
    let offer = SdpOffer::from_sdp_string(&offer.to_sdp_string()).expect("offer parses");
    let answer = r.rtc.sdp_api().accept_offer(offer).expect("answer");
    l.rtc.sdp_api().accept_answer(pending, answer).expect("accept answer");
    l.drain();
    r.drain();

    l.rtc.bwe().set_bitrate(Bitrate::kbps(300), Bitrate::mbps(2));
    l.drain();

    let step = Duration::from_millis(1);
    let total = Duration::from_secs(seconds);
    let (mut frame, mut packet) = (0u32, 0u32);
    let mut now = start;
    while now - start < total {
        now += step;
        let ms = (now - start).as_millis() as u64;
        if l.deadline <= now {
            l.timeout(now);
        }
        if r.deadline <= now {
            r.timeout(now);
        }
        shuttle(&mut l, &mut r, now);
        if !(l.seen.connected && r.seen.connected) {
            continue;
        }
        if ms % 33 == 0 {
            let rv = r.video.unwrap_or(video);
            for (p, mid) in [(&mut l, video), (&mut r, rv)] {
                let time = MediaTime::from_90khz(ms * 90);
                p.send(mid, Codec::H264, now, time, fake_h264(frame % 60 == 0, frame));
            }
            frame += 1;
        }
        if ms % 20 == 0 {
            let ra = r.audio.unwrap_or(audio);
            for (p, mid) in [(&mut l, audio), (&mut r, ra)] {
                let time = MediaTime::new(ms * 48, str0m::media::Frequency::FORTY_EIGHT_KHZ);
                p.send(mid, Codec::Opus, now, time, vec![packet as u8; 80]);
            }
            packet += 1;
        }
        if ms % 100 == 0 {
            for p in [&mut l, &mut r] {
                if let Some(mut c) = p.channel.and_then(|id| p.rtc.channel(id)) {
                    c.write(false, b"ping").expect("channel write");
                }
                p.drain();
            }
        }
        shuttle(&mut l, &mut r, now);
    }
    (l.seen, r.seen)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An iPhone calls an Android phone, and the other way round: the CryptoKit backend on one
    /// side, RustCrypto (with OUR certificate) on the other. Each must take the other's
    /// certificate and finish the handshake; then media and data flow.
    #[cfg(target_vendor = "apple")]
    #[test]
    fn the_two_crypto_backends_shake_hands() {
        let apple = || Some(Arc::new(str0m_apple_crypto::default_provider()));
        let rust = || Some(Arc::new(str0m_rust_crypto::default_provider()));
        for (who, l, r) in [("apple offers", apple(), rust()), ("rust offers", rust(), apple())] {
            let (l, r) = loopback_with(3, l, r);
            for s in [&l, &r] {
                assert!(s.connected, "{who}: not connected");
                assert!(s.video_frames > 60 && s.audio_frames > 100 && s.channel_messages > 20, "{who}: {s:?}");
            }
        }
    }

    #[test]
    fn profile_runs_end_to_end() {
        let (l, r) = loopback(10);
        println!("L {l:#?}\nR {r:#?}");
        for s in [&l, &r] {
            assert!(s.connected);
            assert!(s.channel_open);
            assert!(s.video_frames > 250, "video frames {}", s.video_frames);
            assert!(s.video_keyframes >= 4, "keyframes {}", s.video_keyframes);
            assert!(s.audio_frames > 400, "audio frames {}", s.audio_frames);
            assert!(s.channel_messages > 80, "messages {}", s.channel_messages);
        }
        assert!(l.bwe_estimates > 0, "no bandwidth estimate on the offering side");
    }

    /// The DTLS certificate is hand-built DER (vendor/str0m-rust-crypto/src/cert.rs). The loopback
    /// parses it and uses its key, but never checks what only a browser or another stack would:
    /// the self-signature (over the TBS as a strict parser re-encodes it, so the DER is canonical),
    /// the key pairing, the serial, the validity.
    #[cfg(feature = "crypto-rust")]
    #[test]
    fn dtls_cert_is_canonical_x509() {
        use p256::ecdsa::signature::Verifier;
        use p256::ecdsa::{DerSignature, SigningKey, VerifyingKey};
        use p256::pkcs8::DecodePrivateKey;
        use x509_cert::der::{Decode, Encode};

        let provider = str0m::crypto::from_feature_flags();
        let a = provider.dtls_provider.generate_certificate().expect("certificate");
        let b = provider.dtls_provider.generate_certificate().expect("certificate");

        let cert = x509_cert::Certificate::from_der(&a.certificate).expect("strict X.509 DER");
        let tbs = cert.tbs_certificate();
        assert_eq!(tbs.version(), x509_cert::certificate::Version::V3);
        assert_eq!(cert.signature_algorithm().oid.to_string(), "1.2.840.10045.4.3.2");
        assert_eq!(tbs.issuer(), tbs.subject());

        let spki = tbs.subject_public_key_info();
        let public = VerifyingKey::from_sec1_bytes(spki.subject_public_key.raw_bytes()).expect("P-256 key");
        let signature = DerSignature::from_bytes(cert.signature().raw_bytes()).expect("ECDSA signature");
        public.verify(&tbs.to_der().unwrap(), &signature).expect("self-signature");
        let key = SigningKey::from_pkcs8_der(&a.private_key).expect("PKCS#8 key");
        assert_eq!(key.verifying_key(), &public);

        let serial = tbs.serial_number().as_bytes();
        assert!(serial.len() == 16 && serial[0] & 0x80 == 0, "serial {serial:02x?}");
        let other = x509_cert::Certificate::from_der(&b.certificate).unwrap();
        assert_ne!(other.tbs_certificate().serial_number(), tbs.serial_number());

        let from = tbs.validity().not_before.to_unix_duration();
        let to = tbs.validity().not_after.to_unix_duration();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
        assert!(from <= now && now - from < Duration::from_secs(60), "not before {from:?}, now {now:?}");
        assert_eq!(to - from, Duration::from_secs(365 * 86_400));
    }
}
