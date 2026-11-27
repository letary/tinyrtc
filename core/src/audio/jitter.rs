//! The audio jitter buffer of one peer: packets arrive when the network lets them, sound must leave
//! at the audio unit's steady rate. Everything between the two is here — reordering, a delay that
//! follows the network's jitter, loss concealment, and the small speed-ups and slow-downs that keep
//! the delay where it should be (the two clocks, the sender's and ours, never agree exactly).
//!
//! The rules it plays by:
//!   - the unit of playout is what the decoder gives for one packet (20 ms in the profile);
//!   - a missing packet is recovered from the NEXT one's in-band FEC when that one is here already,
//!     concealed by the decoder otherwise;
//!   - the target delay is the 95th percentile of the arrival jitter over the last seconds, plus one
//!     frame. Jitter is measured against the fastest packet of the window, so a drifting clock or a
//!     changed route moves the baseline instead of inflating the delay;
//!   - when the buffer holds more than the target for a while, two frames are folded into one (a
//!     cross-fade), at most once per half second: a speed-up of 4%, which a voice survives;
//!   - when a packet is missing while later ones are here and less is held than the target, the
//!     cursor waits for it (up to the target): overtaking packets are what jitter looks like, and
//!     waiting is how the delay grows to meet it;
//!   - with nothing to play the cursor waits, and the wait is counted. When the next packet comes,
//!     the hole before it is first paid from that count: a silent sender (DTX) left a hole as long
//!     as the wait, so the next word lands where it should, at the delay there was; a packet that
//!     was merely late left none, so it is played, and the delay has grown by the wait — the
//!     buffer adapts upward by itself, without dropping what arrives;
//!   - a buffer far over its target (a stalled network letting go at once) skips to the live end
//!     and starts learning again; so does one whose packets keep arriving behind the cursor.
//!
//! No clock is read here: time comes in with the packets, and every `pull` is "now". That makes the
//! buffer a pure function of its inputs — the tests drive it through seconds of network in
//! microseconds.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

/// 20 ms at 48 kHz: the profile's packet, and the unit of concealment.
pub const FRAME: usize = 960;
/// The longest an Opus packet can be: 120 ms.
pub const MAX_PACKET: usize = FRAME * 6;

const RATE: u64 = 48_000;
const MIN_TARGET: usize = FRAME * 2; // 40 ms
const MAX_TARGET: usize = RATE as usize * 6 / 10; // 600 ms
/// Arrivals remembered for the jitter estimate: 10 s of 20 ms packets.
const WINDOW: usize = 500;
/// Over the target by this much, for `PATIENCE` frames in a row, asks for a speed-up.
const OVER: usize = FRAME * 3;
const PATIENCE: u32 = 25;
/// Concealed frames in a row that still sound like something; silence after.
const CONCEAL_RUN: u32 = 5;
/// Packets in a row behind the cursor that mean the timeline moved.
const LATE_RUN: u32 = 10;
/// A hole longer than this is a jump of the sender's clock, not a loss.
const JUMP: u64 = RATE * 2;
/// Holding this much over the target is not drift, it is a burst: skipped, not sped through.
const FLUSH: usize = RATE as usize / 2;

/// What the buffer needs from a decoder (Opus in the library, a counter in the tests).
pub trait FrameDecoder {
    /// Decodes one packet; returns the samples written (`out` holds `MAX_PACKET`), 0 on a bad packet.
    fn decode(&mut self, packet: &[u8], out: &mut [i16]) -> usize;
    /// One frame in place of a lost packet.
    fn conceal(&mut self, out: &mut [i16; FRAME]);
    /// The frame BEFORE `next`, from the redundancy `next` carries; false when it carries none.
    fn decode_fec(&mut self, next: &[u8], out: &mut [i16; FRAME]) -> bool;
}

#[derive(Debug, Clone)]
pub struct Packet {
    pub data: Arc<[u8]>,
    /// Its first sample on the sender's timeline (the RTP time, extended), 48 kHz.
    pub time: u64,
    pub received: Instant,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// The delay aimed for, and the sound held right now, in milliseconds.
    pub target_ms: u32,
    pub level_ms: u32,
    pub decoded: u64,
    pub concealed: u64,
    pub recovered: u64,
    /// Arrived behind the cursor: played as a loss already.
    pub late: u64,
    /// Frames folded away to shorten the delay, and frames waited to lengthen it.
    pub accelerated: u64,
    pub expanded: u64,
    pub restarts: u64,
}

pub struct JitterBuffer<D: FrameDecoder> {
    decoder: D,
    packets: BTreeMap<u64, Packet>,
    /// The sender-timeline position of the next sample to produce; None = filling up.
    cursor: Option<u64>,
    /// Decoded and not pulled yet.
    pcm: VecDeque<i16>,
    /// Transit of the last arrivals, in sender samples (an arbitrary offset: only differences count).
    transits: VecDeque<i64>,
    base: Option<(Instant, u64)>,
    target: usize,
    over: u32,
    since_step: u32,
    conceal_run: u32,
    late_run: u32,
    /// Samples concealed with nothing to play and the cursor standing still.
    starved: u64,
    /// Samples concealed waiting for the packet at the cursor while later ones were here.
    hole_wait: u64,
    stats: Stats,
    scratch: Vec<i16>,
}

impl<D: FrameDecoder> JitterBuffer<D> {
    pub fn new(decoder: D) -> Self {
        JitterBuffer {
            decoder,
            packets: BTreeMap::new(),
            cursor: None,
            pcm: VecDeque::with_capacity(MAX_PACKET * 2),
            transits: VecDeque::with_capacity(WINDOW),
            base: None,
            target: MIN_TARGET,
            over: 0,
            since_step: 0,
            conceal_run: 0,
            late_run: 0,
            starved: 0,
            hole_wait: 0,
            stats: Stats::default(),
            scratch: vec![0; MAX_PACKET],
        }
    }

    pub fn stats(&self) -> Stats {
        let mut stats = self.stats;
        stats.target_ms = (self.target as u64 * 1000 / RATE) as u32;
        stats.level_ms = (self.level() as u64 * 1000 / RATE) as u32;
        stats
    }

    /// The sound held: from the cursor to the end of the newest packet, plus what is decoded.
    fn level(&self) -> usize {
        let ahead = match (self.cursor, self.packets.last_key_value()) {
            (Some(cursor), Some((time, _))) => {
                // The part of the hole before the oldest packet that the wait already paid for
                // (see `produce`) is not sound held.
                let oldest = *self.packets.first_key_value().map(|(t, _)| t).unwrap_or(time);
                let paid = oldest.saturating_sub(cursor).min(self.starved);
                (time + FRAME as u64).saturating_sub(cursor + paid) as usize
            }
            (None, Some((newest, _))) => {
                let oldest = *self.packets.first_key_value().map(|(t, _)| t).unwrap_or(newest);
                (newest + FRAME as u64 - oldest) as usize
            }
            _ => 0,
        };
        ahead + self.pcm.len()
    }

    pub fn push(&mut self, packet: Packet) {
        self.observe(&packet);
        if let Some(cursor) = self.cursor {
            if packet.time < cursor {
                self.stats.late += 1;
                self.late_run += 1;
                if self.late_run >= LATE_RUN {
                    self.restart();
                    self.packets.insert(packet.time, packet);
                }
                return;
            }
        }
        self.late_run = 0;
        self.packets.insert(packet.time, packet);
    }

    /// The timeline is gone (a long stall, a jump of the sender's clock): fill up again.
    fn restart(&mut self) {
        self.packets.clear();
        self.cursor = None;
        self.transits.clear();
        self.base = None;
        self.late_run = 0;
        self.starved = 0;
        self.over = 0;
        self.stats.restarts += 1;
    }

    /// One arrival into the jitter estimate, and the target from it.
    fn observe(&mut self, packet: &Packet) {
        let (base_at, base_time) = *self.base.get_or_insert((packet.received, packet.time));
        let arrived = packet.received.saturating_duration_since(base_at).as_micros() as i64 * RATE as i64 / 1_000_000;
        let transit = arrived - (packet.time as i64 - base_time as i64);
        if self.transits.len() == WINDOW {
            self.transits.pop_front();
        }
        self.transits.push_back(transit);

        let fastest = self.transits.iter().copied().min().unwrap_or(transit);
        let mut jitter: Vec<i64> = self.transits.iter().map(|t| t - fastest).collect();
        let at = (jitter.len() * 95 / 100).min(jitter.len() - 1);
        let (_, p95, _) = jitter.select_nth_unstable(at);
        self.target = (*p95 as usize + FRAME).clamp(MIN_TARGET, MAX_TARGET);
    }

    /// Fills `out` — always all of it: the audio unit does not wait.
    pub fn pull(&mut self, out: &mut [i16]) {
        for sample in out.iter_mut() {
            if self.pcm.is_empty() {
                self.produce();
            }
            *sample = self.pcm.pop_front().unwrap_or(0);
        }
    }

    fn silence(&mut self) {
        self.pcm.extend(std::iter::repeat(0).take(FRAME));
    }

    fn conceal(&mut self) {
        self.conceal_run += 1;
        if self.conceal_run > CONCEAL_RUN {
            return self.silence();
        }
        let mut frame = [0i16; FRAME];
        self.decoder.conceal(&mut frame);
        self.pcm.extend(frame);
    }

    /// Decodes one packet into `scratch`; the samples it gave.
    fn decode(&mut self, packet: &Packet) -> usize {
        let n = self.decoder.decode(&packet.data, &mut self.scratch);
        if n > 0 {
            self.stats.decoded += 1;
            self.conceal_run = 0;
        }
        n
    }

    /// Puts at least one frame into `pcm`.
    fn produce(&mut self) {
        let Some(cursor) = self.cursor else {
            // Filling up: start once the target is held (or the oldest packet waited that long).
            let ready = self.packets.first_key_value().is_some_and(|_| self.level() >= self.target);
            if ready {
                self.cursor = self.packets.first_key_value().map(|(time, _)| *time);
                return self.produce();
            }
            return self.silence();
        };

        // Far more held than aimed for (a stalled network let go of everything at once): no
        // speed-up catches that in useful time. Skip to the live end and learn the network anew.
        if self.level() > self.target + FLUSH {
            return self.skip_to_live();
        }

        self.since_step += 1;
        if let Some(packet) = self.packets.remove(&cursor) {
            // What was waited out with nothing to play is now delay: the buffer grew by itself.
            self.starved = 0;
            self.hole_wait = 0;
            let n = self.decode(&packet);
            if n == 0 {
                self.stats.concealed += 1;
                self.cursor = Some(cursor + FRAME as u64);
                return self.conceal();
            }
            self.cursor = Some(cursor + n as u64);
            if self.wants_faster() {
                return self.accelerate(n);
            }
            self.pcm.extend(&self.scratch[..n]);
            return;
        }

        // Nothing at the cursor. Drop what a jump left behind it, then look at what is ahead.
        while self.packets.first_key_value().is_some_and(|(time, _)| *time < cursor) {
            self.packets.pop_first();
        }
        let Some((next, _)) = self.packets.first_key_value() else {
            // Empty: a silent sender, or packets on their way. The cursor WAITS, and the wait is
            // remembered — which of the two it was shows when the next packet comes.
            self.starved += FRAME as u64;
            if self.conceal_run < CONCEAL_RUN {
                self.stats.concealed += 1;
            }
            return self.conceal();
        };
        let next = *next;
        // The sender's timeline moved on while we waited: that much of the hole before `next` was
        // the wait itself, not loss. A pause (DTX) is all wait — the cursor lands on `next` and the
        // delay is what it was. A late packet has no hole — it is played, and the delay has grown.
        let credit = (next - cursor).min(self.starved);
        self.starved = 0;
        if credit > 0 {
            self.cursor = Some(cursor + credit);
            return self.produce();
        }
        if next - cursor > JUMP {
            self.cursor = Some(next);
            return self.produce();
        }
        // A hole before `next`, with less held than aimed for: the missing packet is more likely
        // late than lost (packets overtaking each other is what jitter looks like). Wait for it —
        // the cursor stands, the delay grows towards the target — but no longer than the target.
        if self.level() < self.target && (self.hole_wait as usize) < self.target {
            self.hole_wait += FRAME as u64;
            self.stats.expanded += 1;
            let mut frame = [0i16; FRAME];
            self.decoder.conceal(&mut frame);
            self.pcm.extend(frame);
            return;
        }
        self.hole_wait = 0;
        // Lost. The frame right before `next` may ride in its redundancy.
        self.cursor = Some(cursor + FRAME as u64);
        if next == cursor + FRAME as u64 {
            let data = self.packets[&next].data.clone();
            let mut frame = [0i16; FRAME];
            if self.decoder.decode_fec(&data, &mut frame) {
                self.stats.recovered += 1;
                self.conceal_run = 0;
                self.pcm.extend(frame);
                return;
            }
        }
        self.stats.concealed += 1;
        self.conceal();
    }

    /// Drops everything older than the target before the newest packet, and forgets the arrival
    /// history: the burst that brought us here says nothing about the network's steady jitter.
    fn skip_to_live(&mut self) {
        self.transits.clear();
        self.base = None;
        self.target = MIN_TARGET;
        self.over = 0;
        self.starved = 0;
        self.stats.restarts += 1;
        let Some(newest) = self.packets.last_key_value().map(|(time, _)| *time) else { return };
        let from = (newest + FRAME as u64).saturating_sub(self.target as u64);
        self.packets = self.packets.split_off(&from);
        self.cursor = self.packets.first_key_value().map(|(time, _)| *time);
        self.produce()
    }

    fn wants_faster(&mut self) -> bool {
        self.over = if self.level() > self.target + OVER { self.over + 1 } else { 0 };
        self.over >= PATIENCE && self.since_step >= PATIENCE
    }

    /// The frame just decoded (`n` samples in `scratch`) and the one after it, cross-faded into
    /// the length of one: 20 ms of delay taken out.
    fn accelerate(&mut self, n: usize) {
        let first: Vec<i16> = self.scratch[..n].to_vec();
        let following = self.cursor.and_then(|cursor| self.packets.remove(&cursor));
        let Some(packet) = following else {
            self.pcm.extend(first);
            return;
        };
        let m = self.decode(&packet);
        if m == 0 || m != n {
            // Not two frames of one length: play both as they are.
            self.pcm.extend(first);
            self.pcm.extend(&self.scratch[..m]);
            self.cursor = self.cursor.map(|cursor| cursor + m.max(FRAME) as u64);
            return;
        }
        self.cursor = self.cursor.map(|cursor| cursor + m as u64);
        for i in 0..n {
            let w = i as i32 * 256 / n as i32;
            let mixed = (first[i] as i32 * (256 - w) + self.scratch[i] as i32 * w) >> 8;
            self.pcm.push_back(mixed as i16);
        }
        self.stats.accelerated += 1;
        self.over = 0;
        self.since_step = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A decoder whose sound is the packet's number: every decoded frame is that number repeated,
    /// a concealed frame is -1, a recovered one is the number below the packet's, negated.
    struct Marks {
        fec: bool,
    }

    fn number(packet: &[u8]) -> i16 {
        i16::from_le_bytes([packet[0], packet[1]])
    }

    impl FrameDecoder for Marks {
        fn decode(&mut self, packet: &[u8], out: &mut [i16]) -> usize {
            if packet.len() < 2 {
                return 0;
            }
            out[..FRAME].fill(number(packet));
            FRAME
        }
        fn conceal(&mut self, out: &mut [i16; FRAME]) {
            out.fill(-1);
        }
        fn decode_fec(&mut self, next: &[u8], out: &mut [i16; FRAME]) -> bool {
            out.fill(-(number(next) - 1));
            self.fec
        }
    }

    /// A network and an audio unit in virtual time: packet `n` (n >= 1) is sent at n * 20 ms and
    /// arrives `delay(n)` later (None = lost); the unit pulls a frame every 20 ms.
    struct World {
        buffer: JitterBuffer<Marks>,
        start: Instant,
        /// What was played, a mark per frame.
        played: Vec<i16>,
    }

    impl World {
        fn new(fec: bool) -> World {
            World { buffer: JitterBuffer::new(Marks { fec }), start: Instant::now(), played: Vec::new() }
        }

        fn run(&mut self, frames: std::ops::Range<u32>, delay: impl Fn(u32) -> Option<u32>) {
            // Arrivals in the order of their arrival time, interleaved with the pulls.
            let mut arrivals: Vec<(u64, u32)> = frames
                .clone()
                .filter_map(|n| delay(n).map(|d| (n as u64 * 20_000 + d as u64 * 1000, n)))
                .collect();
            arrivals.sort();
            let mut arrivals = arrivals.into_iter().peekable();
            for tick in frames {
                let now = tick as u64 * 20_000 + 20_000;
                while let Some((at, n)) = arrivals.peek().copied() {
                    if at > now {
                        break;
                    }
                    arrivals.next();
                    self.buffer.push(Packet {
                        data: Arc::from((n as i16).to_le_bytes().as_slice()),
                        time: n as u64 * FRAME as u64,
                        received: self.start + Duration::from_micros(at),
                    });
                }
                let mut frame = [0i16; FRAME];
                self.buffer.pull(&mut frame);
                assert!(frame.iter().all(|s| *s == frame[0]) || self.buffer.stats().accelerated > 0);
                self.played.push(frame[0]);
            }
        }

        fn real(&self) -> Vec<i16> {
            self.played.iter().copied().filter(|m| *m > 0).collect()
        }
    }

    /// A deterministic "random" 0..m.
    fn noise(n: u32, m: u32) -> u32 {
        (n.wrapping_mul(2654435761) >> 7) % m
    }

    #[test]
    fn a_clean_network_plays_everything_in_order_at_the_floor_delay() {
        let mut w = World::new(true);
        w.run(1..501, |_| Some(5));
        let real = w.real();
        assert!(real.windows(2).all(|p| p[1] == p[0] + 1), "in order, nothing skipped");
        assert!(real.len() >= 495, "played {}", real.len());
        let s = w.buffer.stats();
        assert_eq!((s.concealed, s.late, s.restarts), (0, 0, 0), "{s:?}");
        assert_eq!(s.target_ms, 40);
        assert!(s.level_ms <= 80, "{s:?}");
    }

    #[test]
    fn reordered_packets_are_played_in_order() {
        let mut w = World::new(true);
        // Every fourth packet arrives 30 ms late: after its successor.
        w.run(1..501, |n| Some(if n % 4 == 0 { 35 } else { 5 }));
        let real = w.real();
        assert!(real.windows(2).all(|p| p[1] == p[0] + 1), "in order");
        let s = w.buffer.stats();
        assert_eq!(s.late, 0, "{s:?}");
        assert_eq!(s.concealed, 0, "{s:?}");
    }

    #[test]
    fn the_delay_follows_the_jitter_up_and_comes_back_down() {
        let mut w = World::new(true);
        w.run(1..251, |_| Some(5));
        assert_eq!(w.buffer.stats().target_ms, 40);
        // Five seconds of 0..120 ms jitter.
        w.run(251..501, |n| Some(5 + noise(n, 120)));
        let jittery = w.buffer.stats();
        assert!((110..=170).contains(&jittery.target_ms), "{jittery:?}");
        assert_eq!(jittery.restarts, 0, "grown, not started over: {jittery:?}");
        // The onset costs a handful (the delay was 40 ms when the jitter came); 92 before waiting existed.
        assert!(jittery.late <= 10, "what arrives late is played, not dropped: {jittery:?}");
        assert!(jittery.level_ms >= 100, "{jittery:?}");
        let concealed_after_adapting = {
            w.run(501..751, |n| Some(5 + noise(n, 120)));
            let s = w.buffer.stats();
            s.concealed - jittery.concealed
        };
        assert!(concealed_after_adapting <= 3, "adapted, yet {concealed_after_adapting} holes in 5 s");
        // Calm again: once the jitter left the window, the delay is folded back.
        w.run(751..1601, |_| Some(5));
        let calm = w.buffer.stats();
        assert_eq!(calm.target_ms, 40, "{calm:?}");
        assert!(calm.accelerated > 0, "{calm:?}");
        assert!(calm.level_ms <= 100, "{calm:?}");
    }

    #[test]
    fn a_lost_packet_is_recovered_from_the_next_ones_redundancy() {
        let mut w = World::new(true);
        w.run(1..301, |n| if n % 10 == 0 { None } else { Some(5) });
        let s = w.buffer.stats();
        assert_eq!(s.recovered, 29, "{s:?}"); // 10, 20 .. 290: each has its successor in hand
        assert_eq!(s.concealed, 0, "{s:?}");
        // The recovered frame sits exactly where the lost one would have.
        let at = w.played.iter().position(|m| *m == 9).unwrap();
        assert_eq!(&w.played[at..at + 3], &[9, -10, 11]);
    }

    #[test]
    fn without_redundancy_a_loss_is_concealed_and_two_in_a_row_too() {
        let mut w = World::new(false);
        w.run(1..301, |n| if n % 50 == 0 || n % 50 == 1 && n > 1 { None } else { Some(5) });
        let s = w.buffer.stats();
        assert_eq!(s.recovered, 0);
        assert_eq!(s.concealed, 10, "{s:?}"); // 50,51 100,101 .. 250,251
        let at = w.played.iter().position(|m| *m == 49).unwrap();
        assert_eq!(&w.played[at..at + 4], &[49, -1, -1, 52]);
    }

    #[test]
    fn a_silent_sender_keeps_the_timeline_and_the_delay() {
        let mut w = World::new(true);
        // Two seconds of speech, three of nothing (DTX), speech again.
        w.run(1..401, |n| if (101..251).contains(&n) { None } else { Some(5) });
        let s = w.buffer.stats();
        assert_eq!(s.restarts, 0, "{s:?}");
        assert_eq!(s.late, 0, "{s:?}");
        // The first word after the pause is not clipped, and the delay did not grow.
        let real = w.real();
        assert!(real.contains(&251) && real.contains(&100));
        assert!(s.level_ms <= 80, "{s:?}");
        // Concealment fades to silence instead of droning through the pause.
        let pause = &w.played[150..230];
        assert!(pause.iter().all(|m| *m == 0), "silence in the pause: {pause:?}");
    }

    #[test]
    fn a_stall_then_a_burst_starts_over_instead_of_playing_the_past() {
        let mut w = World::new(true);
        // The network freezes for 2 s at packet 100: 100..200 all arrive together at 4.0 s.
        w.run(1..501, |n| Some(if (100..200).contains(&n) { (200 - n) * 20 + 5 } else { 5 }));
        let s = w.buffer.stats();
        assert_eq!(s.restarts, 1, "{s:?}");
        // After the stall the newest sound is played, close to live.
        assert!(w.real().last().copied().unwrap() >= 495);
        assert!(s.level_ms <= 200, "back near live: {s:?}");
    }

    #[test]
    fn a_bad_packet_is_a_loss_not_a_crash() {
        let mut b = JitterBuffer::new(Marks { fec: false });
        let start = Instant::now();
        for n in 1..=10u64 {
            let data: Arc<[u8]> = if n == 5 { Arc::from([0u8].as_slice()) } else { Arc::from((n as i16).to_le_bytes().as_slice()) };
            b.push(Packet { data, time: n * FRAME as u64, received: start + Duration::from_millis(n * 20) });
        }
        let mut out = vec![0i16; FRAME * 10];
        b.pull(&mut out);
        let marks: Vec<i16> = out.chunks(FRAME).map(|f| f[0]).collect();
        assert_eq!(marks, [1, 2, 3, 4, -1, 6, 7, 8, 9, 10]);
        // An odd pull size takes samples, not frames.
        let mut odd = [0i16; 333];
        b.pull(&mut odd);
    }
}
