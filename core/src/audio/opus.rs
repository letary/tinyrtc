//! Opus, 48 kHz mono, 20 ms packets: the encoder and the decoder.
//!
//! A safe face over libopus (vendor/opus, compiled by build.rs). The PCM is what crosses the C
//! ABI: 48 kHz, mono, int16. The encoder always cuts 20 ms frames; the decoder takes whatever a
//! peer sends (SILK, CELT or hybrid, mono or stereo, 2.5 to 120 ms per packet) and always hands
//! back mono. No call panics, whatever the bytes of a packet.

use core::ffi::c_int;
use core::ptr::{self, NonNull};

/// The one sample rate, both ways.
pub const SAMPLE_RATE: u32 = 48_000;
/// One 20 ms frame: what `Encoder::encode` takes and `conceal` / `decode_fec` produce.
pub const FRAME_SAMPLES: usize = 960;
/// The longest packet, 120 ms: size `Decoder::decode`'s output for it.
pub const MAX_PACKET_SAMPLES: usize = 5760;
/// The most one encoded 20 ms frame can take (libopus caps it there); a smaller `out` makes the
/// encoder spend fewer bits on that frame.
pub const MAX_PACKET_BYTES: usize = 1276;
/// A packet of this many bytes or fewer carries no sound: the encoder is in DTX (`set_dtx`). Not
/// sending it is the saving; the receiver conceals the gap either way (decoding such a packet is
/// concealment too). WebRTC sends the first one of a run, so the other side knows, and drops the
/// rest.
pub const DTX_MAX_BYTES: usize = 2;

/// What went wrong, as libopus says it; `code()` is libopus's (negative) error code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// OPUS_BAD_ARG: an argument out of range; here also an empty packet (libopus would take
    /// that as a loss and conceal: `Decoder::conceal` is the call for that).
    BadArg,
    /// OPUS_BUFFER_TOO_SMALL: `out` cannot hold the packet's samples, or any encoded frame.
    BufferTooSmall,
    /// OPUS_INTERNAL_ERROR.
    Internal,
    /// OPUS_INVALID_PACKET: the bytes are not an Opus packet.
    InvalidPacket,
    /// OPUS_UNIMPLEMENTED.
    Unimplemented,
    /// OPUS_INVALID_STATE.
    InvalidState,
    /// OPUS_ALLOC_FAIL: no memory for the state.
    AllocFail,
    /// A code this list does not know.
    Other(i32),
}

impl Error {
    fn from_code(code: c_int) -> Self {
        match code {
            OPUS_BAD_ARG => Error::BadArg,
            OPUS_BUFFER_TOO_SMALL => Error::BufferTooSmall,
            OPUS_INTERNAL_ERROR => Error::Internal,
            OPUS_INVALID_PACKET => Error::InvalidPacket,
            OPUS_UNIMPLEMENTED => Error::Unimplemented,
            OPUS_INVALID_STATE => Error::InvalidState,
            OPUS_ALLOC_FAIL => Error::AllocFail,
            other => Error::Other(other),
        }
    }

    /// libopus's error code (opus_defines.h).
    pub fn code(self) -> i32 {
        match self {
            Error::BadArg => OPUS_BAD_ARG,
            Error::BufferTooSmall => OPUS_BUFFER_TOO_SMALL,
            Error::Internal => OPUS_INTERNAL_ERROR,
            Error::InvalidPacket => OPUS_INVALID_PACKET,
            Error::Unimplemented => OPUS_UNIMPLEMENTED,
            Error::InvalidState => OPUS_INVALID_STATE,
            Error::AllocFail => OPUS_ALLOC_FAIL,
            Error::Other(code) => code,
        }
    }
}

pub type Result<T> = core::result::Result<T, Error>;

/// A libopus return value: a count, or an error code.
fn count(ret: c_int) -> Result<usize> {
    usize::try_from(ret).map_err(|_| Error::from_code(ret))
}

fn status(ret: c_int) -> Result<()> {
    if ret == OPUS_OK {
        Ok(())
    } else {
        Err(Error::from_code(ret))
    }
}

/// The microphone's encoder: 48 kHz, mono, VOIP, 20 ms frames, in-band FEC on, DTX off.
pub struct Encoder {
    st: NonNull<OpusEncoder>,
}

// SAFETY: the state is one heap block libopus allocated, with no thread affinity; libopus keeps no
// global or thread-local mutable state (this build has no NONTHREADSAFE_PSEUDOSTACK).
unsafe impl Send for Encoder {}

impl Encoder {
    /// `bitrate_bps`: libopus clamps it into 500 b/s .. 750 kb/s; 0 is `BadArg`.
    pub fn new(bitrate_bps: u32) -> Result<Self> {
        let mut err: c_int = OPUS_OK;
        // SAFETY: plain values and a valid out-pointer; NULL (with `err` set) on failure.
        let st = unsafe {
            opus_encoder_create(SAMPLE_RATE as i32, 1, OPUS_APPLICATION_VOIP, &mut err)
        };
        let st = NonNull::new(st)
            .ok_or_else(|| Error::from_code(if err < 0 { err } else { OPUS_ALLOC_FAIL }))?;
        // From here Drop frees it, also on the errors below.
        let mut encoder = Encoder { st };
        encoder.set_bitrate(bitrate_bps)?;
        encoder.set_int(OPUS_SET_INBAND_FEC_REQUEST, 1)?;
        Ok(encoder)
    }

    /// One 20 ms frame into `out`; returns the packet's length. `MAX_PACKET_BYTES` always
    /// suffices. A length <= `DTX_MAX_BYTES` is a DTX packet: the caller decides whether to send
    /// it.
    pub fn encode(&mut self, pcm: &[i16; FRAME_SAMPLES], out: &mut [u8]) -> Result<usize> {
        if out.is_empty() {
            return Err(Error::BufferTooSmall);
        }
        let max = i32::try_from(out.len()).unwrap_or(i32::MAX);
        // SAFETY: `pcm` holds one mono frame of FRAME_SAMPLES; `out` has `max` writable bytes.
        let ret = unsafe {
            opus_encode(self.st.as_ptr(), pcm.as_ptr(), FRAME_SAMPLES as c_int, out.as_mut_ptr(), max)
        };
        count(ret)
    }

    /// The target from the next frame on; clamped as in `new`.
    pub fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<()> {
        self.set_int(OPUS_SET_BITRATE_REQUEST, i32::try_from(bitrate_bps).unwrap_or(i32::MAX))
    }

    /// The loss the network is expected to have, 0..=100 %: how much of the bitrate libopus puts
    /// into in-band FEC (a lower-rate copy of the previous frame, recovered with
    /// `Decoder::decode_fec`). 0 = none. FEC exists only in SILK and hybrid frames, and only when
    /// the bitrate clears a per-bandwidth threshold; libopus narrows the bandwidth to clear it.
    pub fn set_expected_loss(&mut self, percent: u8) -> Result<()> {
        self.set_int(OPUS_SET_PACKET_LOSS_PERC_REQUEST, i32::from(percent))
    }

    /// The encoder's effort, 0..=10 (libopus starts at 9): less CPU for a little less quality.
    pub fn set_complexity(&mut self, complexity: u8) -> Result<()> {
        self.set_int(OPUS_SET_COMPLEXITY_REQUEST, i32::from(complexity.min(10)))
    }

    /// Discontinuous transmission: after 200 ms of silence the encoder emits packets of at most
    /// `DTX_MAX_BYTES` (one fuller comfort-noise update every 400 ms).
    pub fn set_dtx(&mut self, on: bool) -> Result<()> {
        self.set_int(OPUS_SET_DTX_REQUEST, i32::from(on))
    }

    /// A SET request that takes one opus_int32.
    fn set_int(&mut self, request: c_int, value: i32) -> Result<()> {
        // SAFETY: every caller passes a SET request whose single argument is an opus_int32, which
        // is what the variadic reads.
        status(unsafe { opus_encoder_ctl(self.st.as_ptr(), request, value) })
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: made by opus_encoder_create, freed once.
        unsafe { opus_encoder_destroy(self.st.as_ptr()) }
    }
}

/// One remote peer's decoder: whatever the packets carry, out comes 48 kHz mono.
pub struct Decoder {
    st: NonNull<OpusDecoder>,
}

// SAFETY: as for Encoder.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Result<Self> {
        let mut err: c_int = OPUS_OK;
        // SAFETY: as in Encoder::new.
        let st = unsafe { opus_decoder_create(SAMPLE_RATE as i32, 1, &mut err) };
        let st = NonNull::new(st)
            .ok_or_else(|| Error::from_code(if err < 0 { err } else { OPUS_ALLOC_FAIL }))?;
        Ok(Decoder { st })
    }

    /// Decodes one packet into `out`; returns the samples written. A packet holds 2.5 to 120 ms,
    /// so size `out` for `MAX_PACKET_SAMPLES` (`BufferTooSmall` if the packet does not fit; then
    /// nothing was decoded). An empty packet is `BadArg`; a loss is `conceal` or `decode_fec`.
    pub fn decode(&mut self, packet: &[u8], out: &mut [i16]) -> Result<usize> {
        if packet.is_empty() {
            return Err(Error::BadArg);
        }
        if out.is_empty() {
            return Err(Error::BufferTooSmall);
        }
        let len = i32::try_from(packet.len()).map_err(|_| Error::InvalidPacket)?;
        // libopus clamps the space to the packet's duration before it allocates anything.
        let space = c_int::try_from(out.len()).unwrap_or(c_int::MAX);
        // SAFETY: `packet` has `len` readable bytes, `out` `space` writable samples (mono).
        count(unsafe { opus_decode(self.st.as_ptr(), packet.as_ptr(), len, out.as_mut_ptr(), space, 0) })
    }

    /// Packet loss concealment for one missing 20 ms frame: the sound carried on (SILK's pitch
    /// extrapolation, CELT's), fading out over consecutive losses.
    pub fn conceal(&mut self, out: &mut [i16; FRAME_SAMPLES]) -> Result<()> {
        // SAFETY: no packet = a loss; `out` has FRAME_SAMPLES writable samples.
        let ret = unsafe {
            opus_decode(self.st.as_ptr(), ptr::null(), 0, out.as_mut_ptr(), FRAME_SAMPLES as c_int, 0)
        };
        count(ret).map(|_| ())
    }

    /// Recovers the PREVIOUS, lost 20 ms frame from the in-band FEC of the packet after it. Then
    /// decode `next_packet` itself as usual. Where the packet carries no FEC for it (a CELT packet,
    /// no expected loss at the sender, or a frame SILK's VAD found quiet) this is concealment.
    pub fn decode_fec(&mut self, next_packet: &[u8], out: &mut [i16; FRAME_SAMPLES]) -> Result<()> {
        if next_packet.is_empty() {
            return Err(Error::BadArg);
        }
        let len = i32::try_from(next_packet.len()).map_err(|_| Error::InvalidPacket)?;
        // SAFETY: as in decode; FEC decodes exactly FRAME_SAMPLES (a multiple of 2.5 ms).
        let ret = unsafe {
            opus_decode(self.st.as_ptr(), next_packet.as_ptr(), len, out.as_mut_ptr(), FRAME_SAMPLES as c_int, 1)
        };
        count(ret).map(|_| ())
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: made by opus_decoder_create, freed once.
        unsafe { opus_decoder_destroy(self.st.as_ptr()) }
    }
}

/// A packet's duration in samples at 48 kHz, without decoding it: from its TOC byte (and frame
/// count). Only the framing that says how long it is gets checked; `decode` checks the rest.
pub fn packet_samples(packet: &[u8]) -> Result<usize> {
    if packet.is_empty() {
        return Err(Error::BadArg);
    }
    let len = i32::try_from(packet.len()).map_err(|_| Error::InvalidPacket)?;
    // SAFETY: `packet` has `len` readable bytes.
    match count(unsafe { opus_packet_get_nb_samples(packet.as_ptr(), len, SAMPLE_RATE as i32) })? {
        // A code-3 packet that says zero frames: libopus counts 0, RFC 6716 forbids it.
        0 => Err(Error::InvalidPacket),
        n => Ok(n),
    }
}

// ---- libopus (include/opus.h, opus_defines.h) ------------------------------------------------------

#[repr(C)]
struct OpusEncoder {
    _opaque: [u8; 0],
}

#[repr(C)]
struct OpusDecoder {
    _opaque: [u8; 0],
}

const OPUS_OK: c_int = 0;
const OPUS_BAD_ARG: c_int = -1;
const OPUS_BUFFER_TOO_SMALL: c_int = -2;
const OPUS_INTERNAL_ERROR: c_int = -3;
const OPUS_INVALID_PACKET: c_int = -4;
const OPUS_UNIMPLEMENTED: c_int = -5;
const OPUS_INVALID_STATE: c_int = -6;
const OPUS_ALLOC_FAIL: c_int = -7;

const OPUS_APPLICATION_VOIP: c_int = 2048;
const OPUS_SET_BITRATE_REQUEST: c_int = 4002;
const OPUS_SET_COMPLEXITY_REQUEST: c_int = 4010;
const OPUS_SET_INBAND_FEC_REQUEST: c_int = 4012;
const OPUS_SET_PACKET_LOSS_PERC_REQUEST: c_int = 4014;
const OPUS_SET_DTX_REQUEST: c_int = 4016;

// opus_int32 is int32_t; `int` is 32 bits on every target here.
extern "C" {
    fn opus_encoder_create(fs: i32, channels: c_int, application: c_int, error: *mut c_int) -> *mut OpusEncoder;
    fn opus_encoder_destroy(st: *mut OpusEncoder);
    fn opus_encode(st: *mut OpusEncoder, pcm: *const i16, frame_size: c_int, data: *mut u8, max_data_bytes: i32) -> i32;
    // Variadic in C, so declared variadic: Apple's arm64 ABI passes variadic arguments differently.
    fn opus_encoder_ctl(st: *mut OpusEncoder, request: c_int, ...) -> c_int;
    fn opus_decoder_create(fs: i32, channels: c_int, error: *mut c_int) -> *mut OpusDecoder;
    fn opus_decoder_destroy(st: *mut OpusDecoder);
    fn opus_decode(st: *mut OpusDecoder, data: *const u8, len: i32, pcm: *mut i16, frame_size: c_int, decode_fec: c_int) -> c_int;
    fn opus_packet_get_nb_samples(packet: *const u8, len: i32, fs: i32) -> c_int;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    // The raw API the tests need beyond the wrapper: a stereo encoder for packets of other kinds.
    const OPUS_APPLICATION_AUDIO: c_int = 2049;
    const OPUS_SET_BANDWIDTH_REQUEST: c_int = 4008;
    const OPUS_BANDWIDTH_WIDEBAND: c_int = 1103;
    const OPUS_BANDWIDTH_FULLBAND: c_int = 1105;
    const OPUS_SET_FORCE_CHANNELS_REQUEST: c_int = 4022;
    const OPUS_GET_LOOKAHEAD_REQUEST: c_int = 4027;
    const OPUS_SET_FORCE_MODE_REQUEST: c_int = 11002; // src/opus_private.h
    const MODE_SILK_ONLY: c_int = 1000;
    const MODE_HYBRID: c_int = 1001;
    const MODE_CELT_ONLY: c_int = 1002;

    extern "C" {
        fn opus_packet_has_lbrr(packet: *const u8, len: i32) -> c_int;
    }

    fn has_lbrr(packet: &[u8]) -> bool {
        // SAFETY: `packet` has `len` readable bytes.
        unsafe { opus_packet_has_lbrr(packet.as_ptr(), packet.len() as i32) == 1 }
    }

    fn lookahead(encoder: &Encoder) -> usize {
        let mut value: i32 = 0;
        // SAFETY: a GET request with its opus_int32 out-pointer.
        let ret = unsafe { opus_encoder_ctl(encoder.st.as_ptr(), OPUS_GET_LOOKAHEAD_REQUEST, &mut value as *mut i32) };
        assert_eq!(ret, OPUS_OK);
        value as usize
    }

    struct Stereo(*mut OpusEncoder);

    impl Stereo {
        fn new(bitrate: i32) -> Self {
            let mut err = 0;
            // SAFETY: plain values; checked below.
            let st = unsafe { opus_encoder_create(48_000, 2, OPUS_APPLICATION_AUDIO, &mut err) };
            assert!(!st.is_null() && err == OPUS_OK);
            let s = Stereo(st);
            s.ctl(OPUS_SET_BITRATE_REQUEST, bitrate);
            s.ctl(OPUS_SET_FORCE_CHANNELS_REQUEST, 2);
            s
        }

        fn ctl(&self, request: c_int, value: c_int) {
            // SAFETY: SET requests with one opus_int32.
            assert_eq!(unsafe { opus_encoder_ctl(self.0, request, value) }, OPUS_OK);
        }

        /// `pcm` interleaved, `pcm.len() / 2` samples per channel.
        fn encode(&self, pcm: &[i16], out: &mut [u8]) -> usize {
            // SAFETY: `pcm` holds frame*2 samples, `out` out.len() bytes.
            let n = unsafe { opus_encode(self.0, pcm.as_ptr(), (pcm.len() / 2) as c_int, out.as_mut_ptr(), out.len() as i32) };
            assert!(n > 0, "stereo encode: {n}");
            n as usize
        }
    }

    impl Drop for Stereo {
        fn drop(&mut self) {
            // SAFETY: made by opus_encoder_create.
            unsafe { opus_encoder_destroy(self.0) }
        }
    }

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
        /// -1..1
        fn uniform(&mut self) -> f64 {
            self.next() as f64 / (1u64 << 31) as f64 - 1.0
        }
    }

    fn sine(freq: f64, amplitude: f64, samples: usize) -> Vec<i16> {
        (0..samples).map(|i| (amplitude * (2.0 * PI * freq * i as f64 / 48_000.0).sin()) as i16).collect()
    }

    /// Something a speech coder treats as speech: a harmonic buzz with a gliding pitch, syllable-
    /// rate loudness and a little noise. Unlike a steady sine, extrapolating it across a lost
    /// frame goes wrong, which is what FEC is for.
    fn voice(samples: usize) -> Vec<i16> {
        let mut noise = Lcg(1);
        let mut phase = 0.0f64;
        (0..samples)
            .map(|i| {
                let t = i as f64 / 48_000.0;
                phase += 2.0 * PI * (160.0 + 50.0 * (2.0 * PI * 0.9 * t).sin()) / 48_000.0;
                let buzz: f64 = (1..=16).map(|k| (k as f64 * phase).sin() / k as f64).sum();
                let loudness = 0.35 + 0.65 * (0.5 - 0.5 * (2.0 * PI * 3.0 * t).cos());
                (4000.0 * loudness * buzz + 300.0 * noise.uniform()).clamp(-32767.0, 32767.0) as i16
            })
            .collect()
    }

    fn rms(x: &[i16]) -> f64 {
        (x.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    /// Sign changes, with hysteresis so codec noise near zero cannot add any.
    fn zero_crossings(x: &[i16]) -> usize {
        let mut positive = x.first().is_none_or(|&s| s >= 0);
        let mut n = 0;
        for &s in x {
            if positive && s < -1000 || !positive && s > 1000 {
                positive = !positive;
                n += 1;
            }
        }
        n
    }

    fn frame(x: &[i16]) -> &[i16; FRAME_SAMPLES] {
        x.try_into().unwrap()
    }

    #[test]
    fn sine_survives_24_kbps() {
        let mut encoder = Encoder::new(24_000).unwrap();
        let mut decoder = Decoder::new().unwrap();
        let input = sine(440.0, 10_000.0, 48_000);
        let (mut packet, mut pcm, mut output) = ([0u8; MAX_PACKET_BYTES], [0i16; MAX_PACKET_SAMPLES], Vec::new());
        for f in input.chunks_exact(FRAME_SAMPLES) {
            let n = encoder.encode(frame(f), &mut packet).unwrap();
            assert_eq!(packet_samples(&packet[..n]), Ok(FRAME_SAMPLES));
            let m = decoder.decode(&packet[..n], &mut pcm).unwrap();
            assert_eq!(m, FRAME_SAMPLES);
            output.extend_from_slice(&pcm[..m]);
        }
        // Past the first 100 ms (the codec's start-up).
        let (input, output) = (&input[4800..], &output[4800..]);
        let (rms_in, rms_out) = (rms(input), rms(output));
        assert!(rms_out > 0.8 * rms_in && rms_out < 1.2 * rms_in, "rms in {rms_in:.0} out {rms_out:.0}");
        let expected = 2.0 * 440.0 * output.len() as f64 / 48_000.0;
        let crossings = zero_crossings(output) as f64;
        assert!((crossings - expected).abs() <= 0.01 * expected, "{crossings} zero crossings, {expected} expected");
    }

    /// What a browser may send: stereo, SILK / hybrid / CELT and the switches between them,
    /// 2.5 to 120 ms per packet. The mono decoder takes all of it.
    #[test]
    fn stereo_and_every_mode_decode_to_mono() {
        let stereo = Stereo::new(64_000);
        let (left, right) = (sine(440.0, 8000.0, 48_000 * 6), sine(660.0, 8000.0, 48_000 * 6));
        let pcm: Vec<i16> = left.iter().zip(&right).flat_map(|(&l, &r)| [l, r]).collect();
        let mut decoder = Decoder::new().unwrap();
        let (mut packet, mut out) = ([0u8; 4000], [0i16; MAX_PACKET_SAMPLES]);
        let mut at = 0; // samples per channel consumed
        let mut modes = [false; 3];
        // (forced mode, samples per packet, packets). The bandwidth is forced too: SILK above
        // wideband becomes hybrid, hybrid at wideband or below becomes SILK.
        let plan = [
            (MODE_SILK_ONLY, 960, 10),
            (MODE_HYBRID, 960, 10),
            (MODE_CELT_ONLY, 960, 10),
            (MODE_SILK_ONLY, 960, 5),
            (MODE_CELT_ONLY, 120, 40),
            (MODE_CELT_ONLY, 480, 10),
            (MODE_SILK_ONLY, 2880, 5),
            (MODE_HYBRID, 1920, 5),
            (MODE_SILK_ONLY, 5760, 3),
        ];
        for (mode, samples, packets) in plan {
            stereo.ctl(OPUS_SET_FORCE_MODE_REQUEST, mode);
            let bandwidth = if mode == MODE_SILK_ONLY { OPUS_BANDWIDTH_WIDEBAND } else { OPUS_BANDWIDTH_FULLBAND };
            stereo.ctl(OPUS_SET_BANDWIDTH_REQUEST, bandwidth);
            let mut decoded = Vec::new();
            for i in 0..packets {
                let n = stereo.encode(&pcm[2 * at..2 * (at + samples)], &mut packet);
                at += samples;
                let p = &packet[..n];
                assert!(p[0] & 0x04 != 0, "not a stereo packet");
                let config = p[0] >> 3;
                let kind = if config < 12 { MODE_SILK_ONLY } else if config < 16 { MODE_HYBRID } else { MODE_CELT_ONLY };
                // Leaving SILK / hybrid for CELT takes one more frame in the old mode (it carries
                // the redundant CELT frame the decoder crossfades with).
                if i > 0 {
                    assert_eq!(kind, mode, "{samples} samples: TOC config {config}");
                }
                modes[(kind - MODE_SILK_ONLY) as usize] = true;
                assert_eq!(packet_samples(p), Ok(samples));
                assert_eq!(decoder.decode(p, &mut out), Ok(samples), "mode {mode}, {samples} samples");
                decoded.extend_from_slice(&out[..samples]);
            }
            // The mix of two 8000-peak sines is ~4000 rms; past the first packet of each run.
            let level = rms(&decoded[samples..]);
            assert!(level > 2000.0, "mode {mode}, {samples} samples: rms {level:.0}");
        }
        assert_eq!(modes, [true; 3], "SILK, hybrid, CELT");
    }

    #[test]
    fn conceal_carries_the_sound_on() {
        let mut encoder = Encoder::new(24_000).unwrap();
        let mut decoder = Decoder::new().unwrap();
        let input = voice(FRAME_SAMPLES * 20);
        let (mut packet, mut last) = ([0u8; MAX_PACKET_BYTES], [0i16; FRAME_SAMPLES]);
        for f in input.chunks_exact(FRAME_SAMPLES) {
            let n = encoder.encode(frame(f), &mut packet).unwrap();
            assert_eq!(decoder.decode(&packet[..n], &mut last), Ok(FRAME_SAMPLES));
        }
        let mut concealed = [0i16; FRAME_SAMPLES];
        decoder.conceal(&mut concealed).unwrap();
        let (before, after) = (rms(&last), rms(&concealed));
        assert!(after > 0.3 * before, "decoded rms {before:.0}, concealed {after:.0}");
    }

    /// In-band FEC is SILK's LBRR: a coarser copy of frame k inside packet k + 1. libopus writes
    /// it only when (src/opus_encoder.c decide_fec, silk/fixed/encode_frame_FIX.c):
    ///   - the frame is SILK or hybrid, never CELT;
    ///   - the expected loss is above 0;
    ///   - the bitrate clears a threshold per bandwidth, NB 12 / MB 14 / WB 16 / SWB 20 / FB 22
    ///     kb/s (+1 kb/s to switch on, -1 to stay on), times 1.25 - min(loss, 25 %); above 5 %
    ///     loss libopus narrows the bandwidth until it clears. Here, 24 kb/s at 20 %: hybrid SWB
    ///     (the SWB threshold is 21 x 1.05 = 22 kb/s);
    ///   - and per frame, SILK's VAD puts the frame's speech activity above 0.3: quiet frames
    ///     carry none. This signal's syllable dips are such frames (about a quarter of the losses
    ///     below), so they are left out of the comparison: there `decode_fec` is concealment.
    #[test]
    fn fec_recovers_a_loss_better_than_concealment() {
        let input = voice(48_000 * 6);
        let mut encoder = Encoder::new(24_000).unwrap();
        encoder.set_expected_loss(20).unwrap();
        let delay = lookahead(&encoder);
        let mut packets = Vec::new();
        let mut buf = [0u8; MAX_PACKET_BYTES];
        for f in input.chunks_exact(FRAME_SAMPLES) {
            let n = encoder.encode(frame(f), &mut buf).unwrap();
            packets.push(buf[..n].to_vec());
        }
        let lost = |k: usize| k >= 20 && k.is_multiple_of(6) && k + 1 < packets.len();
        let (mut clean, mut plc, mut fec) = (Decoder::new().unwrap(), Decoder::new().unwrap(), Decoder::new().unwrap());
        let (mut out, mut concealed, mut recovered) = ([0i16; MAX_PACKET_SAMPLES], [0i16; FRAME_SAMPLES], [0i16; FRAME_SAMPLES]);
        let energy = |a: &[i16], b: &[i16]| -> f64 { a.iter().zip(b).map(|(&a, &b)| (a as f64 - b as f64).powi(2)).sum() };
        // Error energy of the lost frames against the input (delayed by the encoder's lookahead)
        // and against what the decoder would have played without the loss: [plc, fec] each.
        let (mut to_input, mut to_clean, mut losses, mut covered) = ([0.0; 2], [0.0; 2], 0, 0);
        for k in 0..packets.len() {
            assert_eq!(clean.decode(&packets[k], &mut out), Ok(FRAME_SAMPLES));
            if !lost(k) {
                let mut other = [0i16; MAX_PACKET_SAMPLES];
                assert_eq!(plc.decode(&packets[k], &mut other), Ok(FRAME_SAMPLES));
                assert_eq!(fec.decode(&packets[k], &mut other), Ok(FRAME_SAMPLES));
                continue;
            }
            losses += 1;
            plc.conceal(&mut concealed).unwrap();
            fec.decode_fec(&packets[k + 1], &mut recovered).unwrap();
            if !has_lbrr(&packets[k + 1]) {
                continue;
            }
            covered += 1;
            let original = &input[k * FRAME_SAMPLES - delay..][..FRAME_SAMPLES];
            for (i, f) in [&concealed, &recovered].into_iter().enumerate() {
                to_input[i] += energy(f, original);
                to_clean[i] += energy(f, &out[..FRAME_SAMPLES]);
            }
        }
        println!(
            "{covered} of {losses} losses with LBRR; error energy, plc / fec: to the input {:.3e} / {:.3e}, to the loss-free decode {:.3e} / {:.3e}",
            to_input[0], to_input[1], to_clean[0], to_clean[1]
        );
        assert!(covered * 2 >= losses, "LBRR in only {covered} of {losses} next packets");
        assert!(to_input[1] < to_input[0], "to the input: fec {:.3e}, plc {:.3e}", to_input[1], to_input[0]);
        assert!(to_clean[1] < to_clean[0], "to the loss-free decode: fec {:.3e}, plc {:.3e}", to_clean[1], to_clean[0]);
    }

    #[test]
    fn garbage_and_empty_packets_are_errors() {
        let mut decoder = Decoder::new().unwrap();
        let (mut out, mut f) = ([0i16; MAX_PACKET_SAMPLES], [0i16; FRAME_SAMPLES]);
        assert_eq!(decoder.decode(&[], &mut out), Err(Error::BadArg));
        assert_eq!(decoder.decode_fec(&[], &mut f), Err(Error::BadArg));
        assert_eq!(packet_samples(&[]), Err(Error::BadArg));

        // Framing no Opus packet has (RFC 6716 3.4). TOC 0x0B = SILK NB 20 ms, code 3.
        let too_big = [&[0x08u8][..], &[0u8; 1300][..]].concat(); // one frame of 1300 > 1275 bytes
        let bad: [&[u8]; 7] = [
            &[0x0B],             // code 3 without its frame count
            &[0x0B, 0x00],       // code 3, zero frames
            &[0x0B, 0x07, 0, 0], // code 3, 7 x 20 ms > 120 ms
            &[0x09, 1, 2, 3],    // code 1: two equal frames in an odd number of bytes
            &[0x0A, 0xFC],       // code 2: a two-byte length cut short
            &[0x0A, 5, 1, 2],    // code 2: the first frame longer than the packet
            &too_big,
        ];
        for (i, p) in bad.iter().enumerate() {
            assert_eq!(decoder.decode(p, &mut out), Err(Error::InvalidPacket), "decode bad[{i}]");
            assert_eq!(decoder.decode_fec(p, &mut f), Err(Error::InvalidPacket), "decode_fec bad[{i}]");
            if i < 3 {
                assert_eq!(packet_samples(p), Err(Error::InvalidPacket), "packet_samples bad[{i}]");
            }
        }

        // A packet longer than `out` is refused, not truncated.
        let mut encoder = Encoder::new(24_000).unwrap();
        let mut packet = [0u8; MAX_PACKET_BYTES];
        let n = encoder.encode(frame(&sine(440.0, 8000.0, FRAME_SAMPLES)), &mut packet).unwrap();
        assert_eq!(decoder.decode(&packet[..n], &mut out[..480]), Err(Error::BufferTooSmall));
        assert_eq!(decoder.decode(&packet[..n], &mut out), Ok(FRAME_SAMPLES));

        // Random bytes: whatever libopus makes of them, an answer and no crash (many random byte
        // strings are valid Opus framing and decode to something).
        let mut rng = Lcg(7);
        let mut bytes = vec![0u8; 1500];
        for _ in 0..20_000 {
            let len = 1 + (rng.next() as usize % if rng.next().is_multiple_of(4) { 1500 } else { 64 });
            bytes[..len].iter_mut().for_each(|b| *b = rng.next() as u8);
            let p = &bytes[..len];
            let samples = packet_samples(p);
            match decoder.decode(p, &mut out) {
                Ok(n) => {
                    assert!(n <= MAX_PACKET_SAMPLES);
                    assert_eq!(samples, Ok(n));
                }
                Err(e) => assert!(matches!(e, Error::InvalidPacket), "{e:?}"),
            }
            let _ = decoder.decode_fec(p, &mut f);
        }
    }

    #[test]
    fn dtx_silence_encodes_tiny() {
        let mut encoder = Encoder::new(24_000).unwrap();
        encoder.set_dtx(true).unwrap();
        let mut decoder = Decoder::new().unwrap();
        let (mut packet, mut out) = ([0u8; MAX_PACKET_BYTES], [0i16; MAX_PACKET_SAMPLES]);
        let speech = voice(FRAME_SAMPLES * 10);
        for f in speech.chunks_exact(FRAME_SAMPLES) {
            assert!(encoder.encode(frame(f), &mut packet).unwrap() > DTX_MAX_BYTES);
        }
        let silence = [0i16; FRAME_SAMPLES];
        let sizes: Vec<usize> = (0..100).map(|_| encoder.encode(&silence, &mut packet).unwrap()).collect();
        // A DTX packet still decodes (to concealment): sending one is harmless.
        let n = *sizes.last().unwrap();
        assert!(n <= DTX_MAX_BYTES);
        assert_eq!(decoder.decode(&packet[..n], &mut out), Ok(FRAME_SAMPLES));
        // DTX starts after 200 ms of silence; then one fuller update per 400 ms.
        let tiny = sizes[10..].iter().filter(|&&n| n <= DTX_MAX_BYTES).count();
        println!("silence with DTX, bytes per packet: {sizes:?}");
        assert!(tiny * 100 >= 90 * 90, "{tiny} of 90 packets <= {DTX_MAX_BYTES} bytes");
    }

    #[test]
    fn bad_settings_are_errors() {
        assert_eq!(Encoder::new(0).err(), Some(Error::BadArg));
        let mut encoder = Encoder::new(24_000).unwrap();
        assert_eq!(encoder.set_expected_loss(101), Err(Error::BadArg));
        assert_eq!(encoder.encode(&[0; FRAME_SAMPLES], &mut []), Err(Error::BufferTooSmall));
        assert_eq!(Error::from_code(-4).code(), -4);
    }
}
