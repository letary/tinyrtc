//! The sound of a context: ONE microphone in, ONE mix out, whatever the number of peers.
//!
//! Capture: the platform pushes what its audio unit recorded (after the OS's echo canceller). It
//! is cut into 20 ms frames, encoded ONCE, and handed to every taker — a connection that has the
//! microphone on. Silence suppression (Opus DTX) is OFF unless asked for: its voice detector takes
//! any steady sound — a tone, a hum, music held on a note — for silence and cuts it out.
//!
//! Playout: every peer is a stream — a queue the connection's thread puts packets into, and a
//! jitter buffer with its own decoder that lives on the audio thread's side. A pull moves what
//! arrived into the buffers and returns the sum of them all.
//!
//! Both calls take a mutex nobody holds for long (a list changes when a peer comes or goes) and do
//! no I/O: the audio unit's thread is never made to wait for the network.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::jitter::{self, FrameDecoder, JitterBuffer, Packet, FRAME};
use super::opus;

/// Speech, mono, full band: transparent for a voice, and above the rate at which Opus adds the
/// redundancy a lost packet is recovered from.
const BITRATE: u32 = 32_000;
/// The loss the encoder prepares for: it decides how much redundancy rides in each packet.
const EXPECTED_LOSS: u8 = 10;
/// The encoder's effort (0..=10). On a phone 5, as libwebrtc does on Android, iOS and ARM (its
/// kDefaultComplexity): about half the encoder's CPU of libopus's default 9, for a difference a
/// voice does not show. 9 elsewhere, as libwebrtc.
const COMPLEXITY: u8 = if cfg!(any(target_os = "android", target_os = "ios")) { 5 } else { 9 };
/// Packets waiting for the audio thread, per stream: 2 s. More means nobody is pulling.
const QUEUE_LIMIT: usize = 100;

impl FrameDecoder for opus::Decoder {
    fn decode(&mut self, packet: &[u8], out: &mut [i16]) -> usize {
        opus::Decoder::decode(self, packet, out).unwrap_or(0)
    }
    fn conceal(&mut self, out: &mut [i16; FRAME]) {
        if opus::Decoder::conceal(self, out).is_err() {
            out.fill(0);
        }
    }
    fn decode_fec(&mut self, next: &[u8], out: &mut [i16; FRAME]) -> bool {
        opus::Decoder::decode_fec(self, next, out).is_ok()
    }
}

/// One encoded 20 ms of the microphone.
#[derive(Debug, Clone)]
pub struct Encoded {
    pub data: Arc<[u8]>,
    /// The position of its first sample in the capture stream, 48 kHz.
    pub time: u64,
    pub captured: Instant,
}

type Taker = Box<dyn Fn(&Encoded) + Send>;

struct Capture {
    encoder: Option<opus::Encoder>,
    /// Pushed and not yet a whole frame.
    pending: Vec<i16>,
    /// Samples encoded so far: the capture stream's clock.
    time: u64,
    takers: Vec<(u64, Taker)>,
    packet: [u8; opus::MAX_PACKET_BYTES],
}

struct Stream {
    id: u64,
    queue: Arc<Mutex<VecDeque<Packet>>>,
    buffer: JitterBuffer<opus::Decoder>,
    muted: bool,
}

struct Shared {
    capture: Mutex<Capture>,
    streams: Mutex<Vec<Stream>>,
    next_id: Mutex<u64>,
}

#[derive(Clone)]
pub struct Engine {
    shared: Arc<Shared>,
}

/// Where a connection puts the packets of its peer. Dropping it does not remove the stream:
/// `Engine::remove_stream` does.
#[derive(Clone)]
pub struct StreamInput {
    pub id: u64,
    queue: Arc<Mutex<VecDeque<Packet>>>,
}

impl StreamInput {
    pub fn push(&self, packet: Packet) {
        let mut queue = self.queue.lock().unwrap();
        if queue.len() >= QUEUE_LIMIT {
            queue.pop_front();
        }
        queue.push_back(packet);
    }
}

impl Default for Engine {
    fn default() -> Self {
        Engine::new()
    }
}

impl Engine {
    pub fn new() -> Engine {
        let encoder = opus::Encoder::new(BITRATE).ok().map(|mut encoder| {
            let _ = encoder.set_expected_loss(EXPECTED_LOSS);
            let _ = encoder.set_complexity(COMPLEXITY);
            encoder
        });
        Engine {
            shared: Arc::new(Shared {
                capture: Mutex::new(Capture {
                    encoder,
                    // Room for the largest slice an audio unit hands over (4096 frames with the
                    // screen locked on iOS) plus a frame: nothing is allocated on its thread.
                    pending: Vec::with_capacity(8192),
                    time: 0,
                    takers: Vec::new(),
                    packet: [0; opus::MAX_PACKET_BYTES],
                }),
                streams: Mutex::new(Vec::new()),
                next_id: Mutex::new(1),
            }),
        }
    }

    fn id(&self) -> u64 {
        let mut next = self.shared.next_id.lock().unwrap();
        *next += 1;
        *next - 1
    }

    // ---- capture

    /// `taker` gets every encoded frame of the microphone from now on, on the pushing thread.
    pub fn add_taker(&self, taker: impl Fn(&Encoded) + Send + 'static) -> u64 {
        let id = self.id();
        self.shared.capture.lock().unwrap().takers.push((id, Box::new(taker)));
        id
    }

    pub fn remove_taker(&self, id: u64) {
        self.shared.capture.lock().unwrap().takers.retain(|(taker, _)| *taker != id);
    }

    /// With it on, what Opus's detector takes for silence is not sent (a short comfort-noise update
    /// goes out about twice a second instead). Saves most of the bitrate of a listener in a group.
    pub fn set_dtx(&self, on: bool) {
        if let Some(encoder) = self.shared.capture.lock().unwrap().encoder.as_mut() {
            let _ = encoder.set_dtx(on);
        }
    }

    /// What the microphone recorded: any number of samples, 48 kHz mono.
    pub fn push(&self, pcm: &[i16]) {
        let mut guard = self.shared.capture.lock().unwrap();
        let capture = &mut *guard;
        capture.pending.extend_from_slice(pcm);
        let mut taken = 0;
        while capture.pending.len() - taken >= FRAME {
            let frame: &[i16; FRAME] = capture.pending[taken..taken + FRAME].try_into().unwrap();
            taken += FRAME;
            let time = capture.time;
            capture.time += FRAME as u64;
            // With nobody taking it the microphone is not encoded at all.
            if capture.takers.is_empty() {
                continue;
            }
            let Some(encoder) = capture.encoder.as_mut() else { continue };
            let Ok(len) = encoder.encode(frame, &mut capture.packet) else { continue };
            // With DTX on, Opus says "nothing to send" with a packet of a byte or two.
            if len <= opus::DTX_MAX_BYTES {
                continue;
            }
            let encoded = Encoded { data: Arc::from(&capture.packet[..len]), time, captured: Instant::now() };
            for (_, taker) in &capture.takers {
                taker(&encoded);
            }
        }
        capture.pending.drain(..taken);
    }

    // ---- playout

    pub fn add_stream(&self) -> StreamInput {
        let input = StreamInput { id: self.id(), queue: Arc::default() };
        if let Ok(decoder) = opus::Decoder::new() {
            self.shared.streams.lock().unwrap().push(Stream {
                id: input.id,
                queue: input.queue.clone(),
                buffer: JitterBuffer::new(decoder),
                muted: false,
            });
        }
        input
    }

    pub fn remove_stream(&self, id: u64) {
        self.shared.streams.lock().unwrap().retain(|stream| stream.id != id);
    }

    /// A muted stream is still received and decoded (its buffer stays in step): it is only left
    /// out of the mix.
    pub fn set_muted(&self, id: u64, muted: bool) {
        if let Some(stream) = self.shared.streams.lock().unwrap().iter_mut().find(|s| s.id == id) {
            stream.muted = muted;
        }
    }

    pub fn stream_stats(&self, id: u64) -> Option<jitter::Stats> {
        self.shared.streams.lock().unwrap().iter().find(|s| s.id == id).map(|s| s.buffer.stats())
    }

    /// Fills `out` with the mix of every stream: silence where there is none.
    pub fn pull(&self, out: &mut [i16]) {
        out.fill(0);
        let mut streams = self.shared.streams.lock().unwrap();
        let mut one = [0i16; FRAME];
        for stream in streams.iter_mut() {
            {
                let mut queue = stream.queue.lock().unwrap();
                while let Some(packet) = queue.pop_front() {
                    stream.buffer.push(packet);
                }
            }
            for chunk in out.chunks_mut(FRAME) {
                let part = &mut one[..chunk.len()];
                stream.buffer.pull(part);
                if !stream.muted {
                    for (sum, sample) in chunk.iter_mut().zip(part.iter()) {
                        *sum = sum.saturating_add(*sample);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn tone(hz: f64, from: usize, len: usize, amplitude: f64) -> Vec<i16> {
        (from..from + len)
            .map(|i| ((i as f64 * hz * std::f64::consts::TAU / 48_000.0).sin() * amplitude) as i16)
            .collect()
    }

    fn hz(pcm: &[i16]) -> f64 {
        let crossings = pcm.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count();
        crossings as f64 / 2.0 / (pcm.len() as f64 / 48_000.0)
    }

    fn rms(pcm: &[i16]) -> f64 {
        (pcm.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / pcm.len() as f64).sqrt()
    }

    /// A microphone engine whose frames go to streams of `speaker` as if 5 ms of network away.
    /// The clock is the capture stream's own: the test runs as fast as it computes.
    fn wire(microphone: &Engine, speaker: &Engine) -> (u64, StreamInput) {
        let input = speaker.add_stream();
        let start = Instant::now();
        let to = input.clone();
        let taker = microphone.add_taker(move |frame| {
            let received = start + Duration::from_micros(frame.time * 1_000_000 / 48_000 + 5_000);
            to.push(Packet { data: frame.data.clone(), time: frame.time, received });
        });
        (taker, input)
    }

    /// Runs `seconds` of both ends: capture in 441-sample pushes, playout in 480-sample pulls.
    fn run(microphones: &[(&Engine, f64)], speaker: &Engine, from: usize, seconds: f64) -> Vec<i16> {
        let total = (seconds * 48_000.0) as usize;
        let mut played = Vec::with_capacity(total);
        let (mut pushed, mut out) = (0usize, [0i16; 480]);
        while played.len() < total {
            let want = played.len() + 480;
            while pushed < want {
                for (microphone, hz) in microphones {
                    microphone.push(&tone(*hz, from + pushed, 441, if *hz > 0.0 { 8000.0 } else { 0.0 }));
                }
                pushed += 441;
            }
            speaker.pull(&mut out);
            played.extend_from_slice(&out);
        }
        played
    }

    #[test]
    fn a_voice_crosses_from_one_engine_to_another() {
        let (a, b) = (Engine::new(), Engine::new());
        let (_, input) = wire(&a, &b);
        let played = run(&[(&a, 440.0)], &b, 0, 2.0);
        let steady = &played[48_000..];
        assert!((hz(steady) - 440.0).abs() < 5.0, "{} Hz", hz(steady));
        assert!((rms(steady) - 8000.0 / 2f64.sqrt()).abs() < 1500.0, "rms {}", rms(steady));
        let stats = b.stream_stats(input.id).unwrap();
        assert_eq!((stats.concealed, stats.late, stats.restarts), (0, 0, 0), "{stats:?}");
        assert!(stats.level_ms <= 100, "{stats:?}");
    }

    #[test]
    fn two_peers_are_mixed_and_a_muted_one_is_left_out() {
        let (a, b, listener) = (Engine::new(), Engine::new(), Engine::new());
        let (_, from_a) = wire(&a, &listener);
        let (_, from_b) = wire(&b, &listener);
        let both = run(&[(&a, 300.0), (&b, 1000.0)], &listener, 0, 2.0);
        let both = &both[48_000..];
        // Two tones of the same level: the mix carries the power of both.
        assert!((rms(both) - 8000.0).abs() < 1500.0, "rms {}", rms(both));

        listener.set_muted(from_a.id, true);
        let one = run(&[(&a, 300.0), (&b, 1000.0)], &listener, 96_000, 1.0);
        let one = &one[24_000..];
        assert!((hz(one) - 1000.0).abs() < 15.0, "{} Hz", hz(one));

        listener.remove_stream(from_b.id);
        let none = run(&[(&a, 300.0), (&b, 1000.0)], &listener, 144_000, 0.5);
        assert!(none[4800..].iter().all(|s| *s == 0), "a muted stream and a removed one: silence");
    }

    #[test]
    fn silence_is_sent_unless_dtx_is_on_and_nobody_listening_costs_no_encoding() {
        let engine = Engine::new();
        let (tx, rx) = mpsc::channel();
        let taker = engine.add_taker(move |frame| {
            let _ = tx.send(frame.data.len());
        });
        // By default every frame goes out, sound or not.
        for i in 0..50 {
            engine.push(&tone(440.0, i * 960, 960, 8000.0));
        }
        for _ in 0..50 {
            engine.push(&[0i16; 960]);
        }
        assert_eq!(rx.try_iter().count(), 100);
        // With DTX, three seconds of silence are a handful of comfort-noise updates.
        engine.set_dtx(true);
        for _ in 0..150 {
            engine.push(&[0i16; 960]);
        }
        let silence = rx.try_iter().count();
        assert!(silence <= 20, "{silence} packets for 3 s of silence");

        engine.remove_taker(taker);
        engine.push(&tone(440.0, 0, 9600, 8000.0));
        assert_eq!(rx.try_iter().count(), 0);
    }
}
