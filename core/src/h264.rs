//! H.264 bitstream framing: Annex B and AVCC in, NAL units out.
//!
//! The platform encoders hand the core one access unit (every NAL unit of one picture) per frame,
//! in their own framing (`tinyrtc_h264_framing`): Annex B from MediaCodec, 4-byte big-endian lengths
//! (AVCC) from VideoToolbox. str0m packetizes and depacketizes; this module only reframes. What it
//! does follows from what str0m 0.26.0 does (paths in that crate):
//!
//! (a) WHAT THE WRITER WANTS. `Writer::write` (src/media/writer.rs:182) queues the bytes as one
//!     sample; `Media::do_payload` (src/media/mod.rs:587) hands it to the `Payloader` kept per
//!     payload type for the life of the media line (src/media/mod.rs:511), and
//!     `H264Packetizer::packetize` (src/packet/h264.rs:237) splits it on Annex B start codes:
//!     `next_ind` (h264.rs:113) takes a `01` after two or more zeros, the whole zero run being the
//!     start code. So 3- and 4-byte start codes, leading_zero_8bits and the trailing_zero_8bits
//!     BETWEEN NAL units are all fine. Not fine: the zeros after the LAST NAL unit stay in it
//!     (h264.rs:262); bytes before the first start code are dropped without a word (h264.rs:248);
//!     a buffer with no start code at all goes out whole as ONE NAL unit (h264.rs:245). There is
//!     no AVCC mode: an AVCC buffer either holds no `00 00 01` and goes out as one "NAL unit" of
//!     type 0 (its first length byte) — rejected by the receiving str0m when it fits one packet
//!     (h264.rs:405), handed on as a type-0 unit when it went as FU-A (h264.rs:365) —, or a length
//!     of 1 (`00 00 00 01`) or of 256..=511 (`00 00 01 xx`) reads as a start code and cuts it at a
//!     random place. Then, per NAL unit (`emit`, h264.rs:128): empty ones are skipped;
//!     AUD (9) is DROPPED; SPS and PPS are held and go out as one STAP-A in front of the next
//!     other NAL unit once both are held (h264.rs:136-163; the hold spans writes, since the
//!     packetizer lives as long as the line); a NAL unit that fits the budget is one packet, a
//!     larger one FU-A (h264.rs:166-231). The budget is the MTU less the 16-byte SRTP tag, rounded
//!     down to 16 (src/media/mod.rs:620-631): 1120 bytes with the default MTU of 1150
//!     (str0m-proto 0.8.0, src/lib.rs:7). A STAP-A over the budget is dropped silently
//!     (h264.rs:155). The marker bit goes on the last packet of the write (h264.rs:270,
//!     src/packet/payload.rs:88-94): one write must be one whole access unit.
//!
//! (b) WHAT THE READER GETS. The Rtc always builds `H264Depacketizer::default()`
//!     (src/packet/mod.rs:422, from src/media/mod.rs:427), so its `is_avc` (h264.rs:284) is false:
//!     that field is reachable only on the standalone type behind the `unversioned` feature
//!     (src/lib.rs:2362), never for the Rtc's own buffers. `MediaData.data` is therefore always
//!     Annex B, the 4-byte start code (h264.rs:49) before every NAL unit and nothing else in
//!     between: a single-NAL packet as it came (h264.rs:313-328), each unit of a STAP-A on its own
//!     — the SPS and the PPS come out as two NAL units (h264.rs:329-363) —, an FU-A reassembled
//!     under its rebuilt header (h264.rs:365-404). AVC output would buy nothing: the event hands
//!     the platform a NAL list either way, and splitting str0m's output on start codes is exact,
//!     since a NAL unit never holds `00 00 01` (emulation prevention) nor ends in 0x00 (H.264
//!     7.4.1).
//!
//! So: OUT, AVCC is rewritten (each length becomes `00 00 00 01`, the same size; a unit that holds
//! a start code or ends in a zero cannot make that move intact and stops it, see `to_packetizer`),
//! and Annex B is copied as it is from its first NAL unit to the end of its last, behind one start
//! code: between those, str0m and `nals` split identically, so those bytes need no rewriting, and
//! what is left out is exactly where the two differ (whatever is before the first unit, the zeros
//! after the last). The copy costs nothing extra: `Writer::write` takes owned bytes
//! (`Into<Arc<[u8]>>`). IN, `nals(&media_data.data, Framing::AnnexB)`.
//!
//! Also true of str0m, for the rest of the core: `MediaData::is_keyframe()`
//! (src/media/event.rs:289) means "an IDR slice somewhere in the sample" (h264.rs:313-319,
//! 347-352, 383-388), with no look at SPS / PPS; and a payload it cannot depacketize (packet type
//! 0, 25..=27 or 29..=31, a STAP-A length past the packet) comes out of `poll_output` as
//! `Err(RtcError::Packet)` (src/lib.rs:1860) after the frame's packets were thrown away
//! (src/packet/buffer_rx.rs:434-442): the Rtc lives on, so that error is a lost frame to log, not
//! a reason to stop polling.
//!
//! Nothing here parses an SPS or touches emulation prevention bytes: a NAL unit is passed on byte
//! for byte. Nothing allocates but `to_packetizer`, into the caller's buffer.

use std::iter::FusedIterator;

/// How an access unit is framed. The numbers are those of `tinyrtc_h264_framing`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    /// Start codes: `00 00 01` or `00 00 00 01` before each NAL unit (MediaCodec).
    AnnexB = 0,
    /// A 4-byte big-endian length before each NAL unit (VideoToolbox).
    Avcc = 1,
}

/// nal_unit_type values (H.264 table 7-1) the core looks at.
pub const SLICE: u8 = 1;
pub const IDR: u8 = 5;
pub const SEI: u8 = 6;
pub const SPS: u8 = 7;
pub const PPS: u8 = 8;
pub const AUD: u8 = 9;

/// The Annex B start code str0m writes before every NAL unit, and so does `to_packetizer`.
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// One NAL unit: the header byte first, no start code and no length in front. Never empty when
/// it comes from `nals`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nal<'a> {
    pub data: &'a [u8],
}

impl Nal<'_> {
    /// nal_unit_type: the low 5 bits of the header byte (0, "unspecified", for an empty one).
    pub fn kind(&self) -> u8 {
        self.data.first().map_or(0, |header| header & 0x1f)
    }
}

/// The NAL units of an access unit, in order. Never reads outside `data`, whatever it holds.
///
/// Annex B: both start codes, any zeros before the first one (leading_zero_8bits) and the zeros
/// after a NAL unit (trailing_zero_8bits) are framing and stay out of the units; an empty unit
/// (two start codes in a row) is skipped. Bytes before the first start code that are not zeros
/// are skipped and make the walk `malformed`, and so does a buffer with no start code at all.
///
/// AVCC: a length that is cut off, zero, or runs past the end ends the walk there, `malformed`;
/// the units before it were yielded. (A zero length is not skipped over: it means the framing is
/// not what it was said to be, e.g. a 2-byte length size or Annex B bytes, and what follows it is
/// not trustworthy.)
pub fn nals(data: &[u8], framing: Framing) -> Nals<'_> {
    Nals { rest: data, framing, started: false, malformed: false }
}

/// The walk of `nals`.
///
/// Why a flag and not `Result` items: an access unit is one decision (send it or hand it on, and
/// log once), malformation is a property of the whole buffer, and the walk stops at the damage
/// anyway. `Result` items would make every consumer handle an error per unit, `?` would not fit
/// the `any` / `map` uses, and a plain `for` would quietly drop the error. So the items are plain
/// `Nal`s, and whoever cares asks the walk once it has ended:
/// `let mut walk = nals(..); for nal in &mut walk { .. } if walk.malformed() { .. }`.
#[derive(Clone, Debug)]
pub struct Nals<'a> {
    /// What is left to walk. Annex B: once `started`, it begins right after a start code.
    rest: &'a [u8],
    framing: Framing,
    started: bool,
    malformed: bool,
}

impl<'a> Nals<'a> {
    /// The walk met bytes that are not NAL units (see `nals`). Complete once `next` returned
    /// `None`; before that it only covers what was walked.
    pub fn malformed(&self) -> bool {
        self.malformed
    }

    fn next_annex_b(&mut self) -> Option<&'a [u8]> {
        let mut rest = self.rest;
        if !self.started {
            self.started = true;
            let before = match start_code(rest) {
                Some((zeros, after)) => {
                    let before = &rest[..zeros];
                    rest = &rest[after..];
                    before
                }
                None => std::mem::take(&mut rest),
            };
            if before.iter().any(|&b| b != 0) {
                self.malformed = true;
            }
        }
        while !rest.is_empty() {
            let (end, next) = start_code(rest).unwrap_or((rest.len(), rest.len()));
            let unit = trim_trailing_zeros(&rest[..end]);
            rest = &rest[next..];
            if !unit.is_empty() {
                self.rest = rest;
                return Some(unit);
            }
        }
        self.rest = rest;
        None
    }

    fn next_avcc(&mut self) -> Option<&'a [u8]> {
        let rest = std::mem::take(&mut self.rest);
        if rest.is_empty() {
            return None;
        }
        if let [a, b, c, d, body @ ..] = rest {
            let len = u32::from_be_bytes([*a, *b, *c, *d]) as usize;
            if len != 0 && len <= body.len() {
                let (unit, after) = body.split_at(len);
                self.rest = after;
                return Some(unit);
            }
        }
        // Cut off, zero, or past the end: `rest` stays empty, the walk is over.
        self.malformed = true;
        None
    }
}

impl<'a> Iterator for Nals<'a> {
    type Item = Nal<'a>;

    fn next(&mut self) -> Option<Nal<'a>> {
        let data = match self.framing {
            Framing::AnnexB => self.next_annex_b(),
            Framing::Avcc => self.next_avcc(),
        }?;
        Some(Nal { data })
    }
}

impl FusedIterator for Nals<'_> {}

/// The first `00 00 01` in `data`: where its two zeros start, and where the bytes after it start.
/// Skips three bytes at a time where the `01` cannot be.
fn start_code(data: &[u8]) -> Option<(usize, usize)> {
    let mut i = 2;
    while i < data.len() {
        match data[i] {
            0 => i += 1,
            1 if data[i - 1] == 0 && data[i - 2] == 0 => return Some((i - 2, i + 1)),
            // Neither here nor at the next two bytes can a `01` end a start code: each would need
            // a zero where this byte is.
            _ => i += 3,
        }
    }
    None
}

/// A unit without the zeros that follow it: the extra zero of a 4-byte start code, or
/// trailing_zero_8bits. A NAL unit never ends in 0x00 (H.264 7.4.1), so nothing of it is cut.
fn trim_trailing_zeros(unit: &[u8]) -> &[u8] {
    let end = unit.iter().rposition(|&b| b != 0).map_or(0, |last| last + 1);
    &unit[..end]
}

/// Where `inner`, a sub-slice of `outer`, starts in it.
fn offset_in(outer: &[u8], inner: &[u8]) -> usize {
    inner.as_ptr() as usize - outer.as_ptr() as usize
}

/// The access unit as str0m's `Writer::write` wants it (module header, (a)): Annex B, nothing
/// before the first start code, nothing after the last NAL unit. `out` is cleared first, so one
/// buffer serves every frame; it is empty when there are no units (nothing to write). Returns
/// true when `out` holds every unit `nals` yields, each one exactly as str0m will cut it out.
///
/// False when the input was malformed (`Nals::malformed`), and for AVCC also when a unit could
/// not survive the move to start codes: taken by its length, it can hold what Annex B cannot
/// carry, a `00 00 01` (str0m would cut it in two) or a zero at its end (it would be trimmed). No
/// valid NAL unit does (H.264 7.4.1), so this is a wrong framing or a broken encoder, and the
/// rewrite stops there. Either way the units before the damage are in `out`; whether to send
/// them is the caller's call.
pub fn to_packetizer(data: &[u8], framing: Framing, out: &mut Vec<u8>) -> bool {
    out.clear();
    let mut walk = nals(data, framing);
    match framing {
        Framing::Avcc => {
            out.reserve(data.len());
            for nal in &mut walk {
                if start_code(nal.data).is_some() || nal.data.last() == Some(&0) {
                    return false;
                }
                out.extend_from_slice(&START_CODE);
                out.extend_from_slice(nal.data);
            }
        }
        Framing::AnnexB => {
            // From the first unit to the end of the last: str0m splits those bytes as `nals` does.
            let mut span: Option<(usize, usize)> = None;
            for nal in &mut walk {
                let start = offset_in(data, nal.data);
                let first = span.map_or(start, |(first, _)| first);
                span = Some((first, start + nal.data.len()));
            }
            if let Some((first, end)) = span {
                out.reserve(START_CODE.len() + end - first);
                out.extend_from_slice(&START_CODE);
                out.extend_from_slice(&data[first..end]);
            }
        }
    }
    !walk.malformed()
}

/// The access unit holds an IDR slice. (str0m's receive-side `is_keyframe` means the same.)
pub fn is_keyframe(data: &[u8], framing: Framing) -> bool {
    nals(data, framing).any(|nal| nal.kind() == IDR)
}

/// The access unit holds both an SPS and a PPS: a keyframe without them cannot be decoded by a
/// receiver that just joined. (Presence only; their order and contents are not looked at.)
pub fn has_parameter_sets(data: &[u8], framing: Framing) -> bool {
    let (mut sps, mut pps) = (false, false);
    for nal in nals(data, framing) {
        match nal.kind() {
            SPS => sps = true,
            PPS => pps = true,
            _ => {}
        }
        if sps && pps {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS_UNIT: &[u8] = &[0x67, 0x42, 0xe0, 0x1f, 0x8d, 0x68, 0x05, 0x00, 0x5b, 0xa1];
    const PPS_UNIT: &[u8] = &[0x68, 0xce, 0x3c, 0x80];
    // Emulation prevention inside (00 00 03), a lone zero, ends in a non-zero byte.
    const IDR_UNIT: &[u8] = &[0x65, 0x88, 0x00, 0x00, 0x03, 0x01, 0x20, 0x00, 0x7f, 0x80];
    const P_UNIT: &[u8] = &[0x41, 0x9a, 0x00, 0x00, 0x03, 0x00, 0x10];
    const SEI_UNIT: &[u8] = &[0x06, 0x05, 0x01, 0xaa, 0x80];
    const AUD_UNIT: &[u8] = &[0x09, 0xf0];

    fn annex_b(units: &[&[u8]], code: &[u8]) -> Vec<u8> {
        units.iter().flat_map(|u| code.iter().chain(u.iter())).copied().collect()
    }

    fn avcc(units: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for u in units {
            out.extend_from_slice(&(u.len() as u32).to_be_bytes());
            out.extend_from_slice(u);
        }
        out
    }

    /// Every unit, and whether the walk was malformed.
    fn walk(data: &[u8], framing: Framing) -> (Vec<&[u8]>, bool) {
        let mut walk = nals(data, framing);
        let units = (&mut walk).map(|n| n.data).collect();
        (units, walk.malformed())
    }

    fn packetized(data: &[u8], framing: Framing) -> (Vec<u8>, bool) {
        let mut out = vec![0xee; 3]; // not empty: to_packetizer clears it
        let clean = to_packetizer(data, framing, &mut out);
        (out, clean)
    }

    #[test]
    fn kind_is_the_low_five_bits() {
        assert_eq!(Nal { data: &[0x67] }.kind(), SPS);
        assert_eq!(Nal { data: &[0x68] }.kind(), PPS);
        assert_eq!(Nal { data: &[0x65, 1] }.kind(), IDR);
        assert_eq!(Nal { data: &[0x25] }.kind(), IDR); // nal_ref_idc 1
        assert_eq!(Nal { data: &[0x01] }.kind(), SLICE); // nal_ref_idc 0
        assert_eq!(Nal { data: &[0x41] }.kind(), SLICE);
        assert_eq!(Nal { data: &[0x06] }.kind(), SEI);
        assert_eq!(Nal { data: &[0x09] }.kind(), AUD);
        assert_eq!(Nal { data: &[0xfc] }.kind(), 28); // forbidden bit set: still only the low bits
        assert_eq!(Nal { data: &[] }.kind(), 0);
    }

    #[test]
    fn annex_b_four_and_three_byte_start_codes() {
        let units = [SPS_UNIT, PPS_UNIT, IDR_UNIT];
        for code in [&[0u8, 0, 0, 1][..], &[0, 0, 1]] {
            let data = annex_b(&units, code);
            assert_eq!(walk(&data, Framing::AnnexB), (units.to_vec(), false));
            let kinds: Vec<u8> = nals(&data, Framing::AnnexB).map(|n| n.kind()).collect();
            assert_eq!(kinds, [SPS, PPS, IDR]);
        }
        // Mixed, the way encoders do it (4 bytes before the first unit, 3 before slices).
        let mut data = vec![0, 0, 0, 1];
        data.extend_from_slice(SPS_UNIT);
        data.extend_from_slice(&[0, 0, 1]);
        data.extend_from_slice(PPS_UNIT);
        data.extend_from_slice(&[0, 0, 0, 1]);
        data.extend_from_slice(IDR_UNIT);
        assert_eq!(walk(&data, Framing::AnnexB), (units.to_vec(), false));
    }

    #[test]
    fn annex_b_leading_and_trailing_zeros_are_framing() {
        // leading_zero_8bits before the first start code.
        let mut data = vec![0, 0, 0, 0, 0];
        data.extend(annex_b(&[SPS_UNIT, P_UNIT], &[0, 0, 1]));
        assert_eq!(walk(&data, Framing::AnnexB), (vec![SPS_UNIT, P_UNIT], false));

        // trailing_zero_8bits after a unit, before the next start code and at the very end.
        let mut data = annex_b(&[SPS_UNIT], &[0, 0, 0, 1]);
        data.extend_from_slice(&[0, 0, 0, 0, 0, 0, 1]);
        data.extend_from_slice(PPS_UNIT);
        data.extend_from_slice(&[0, 0, 1]);
        data.extend_from_slice(IDR_UNIT);
        data.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(walk(&data, Framing::AnnexB), (vec![SPS_UNIT, PPS_UNIT, IDR_UNIT], false));

        // A start code cut off at the end is only zeros: trimmed, not a unit.
        let mut data = annex_b(&[P_UNIT], &[0, 0, 1]);
        data.extend_from_slice(&[0, 0]);
        assert_eq!(walk(&data, Framing::AnnexB), (vec![P_UNIT], false));
        // A whole start code at the end opens an empty unit: skipped.
        data.push(1);
        assert_eq!(walk(&data, Framing::AnnexB), (vec![P_UNIT], false));
    }

    #[test]
    fn annex_b_empty_units_are_skipped() {
        let mut data = vec![0, 0, 1, 0, 0, 1, 0, 0, 0, 1];
        data.extend(annex_b(&[SPS_UNIT], &[0, 0, 1]));
        data.extend_from_slice(&[0, 0, 1, 0, 0, 0, 1]);
        data.extend_from_slice(PPS_UNIT);
        assert_eq!(walk(&data, Framing::AnnexB), (vec![SPS_UNIT, PPS_UNIT], false));
    }

    #[test]
    fn annex_b_malformed() {
        // Junk before the first start code: skipped, reported.
        let mut data = vec![0xde, 0xad, 0];
        data.extend(annex_b(&[SPS_UNIT, PPS_UNIT], &[0, 0, 0, 1]));
        assert_eq!(walk(&data, Framing::AnnexB), (vec![SPS_UNIT, PPS_UNIT], true));

        // No start code at all (a raw unit, or AVCC said to be Annex B).
        assert_eq!(walk(IDR_UNIT, Framing::AnnexB), (vec![], true));
        assert_eq!(walk(&avcc(&[SPS_UNIT, PPS_UNIT]), Framing::AnnexB), (vec![], true));
        assert_eq!(walk(&[0, 0, 2, 0x65], Framing::AnnexB), (vec![], true));

        // Nothing, or only zeros: no units, nothing lost.
        assert_eq!(walk(&[], Framing::AnnexB), (vec![], false));
        assert_eq!(walk(&[0, 0, 0], Framing::AnnexB), (vec![], false));
        assert_eq!(walk(&[0, 0, 1], Framing::AnnexB), (vec![], false));
    }

    #[test]
    fn avcc_well_formed() {
        let units = [SPS_UNIT, PPS_UNIT, SEI_UNIT, IDR_UNIT];
        let data = avcc(&units);
        assert_eq!(walk(&data, Framing::Avcc), (units.to_vec(), false));
        // A unit made of zeros and start-code look-alikes is still taken by its length.
        let odd: &[u8] = &[0x41, 0, 0, 1, 0, 0, 0, 1];
        assert_eq!(walk(&avcc(&[odd]), Framing::Avcc), (vec![odd], false));
        assert_eq!(walk(&[], Framing::Avcc), (vec![], false));
    }

    #[test]
    fn avcc_malformed_ends_the_walk() {
        let good = avcc(&[SPS_UNIT, PPS_UNIT]);
        // The length itself cut off: 1, 2 or 3 bytes left.
        for extra in [&[0u8][..], &[0, 0], &[0, 0, 0]] {
            let mut data = good.clone();
            data.extend_from_slice(extra);
            assert_eq!(walk(&data, Framing::Avcc), (vec![SPS_UNIT, PPS_UNIT], true));
        }
        // The length runs past the end: the unit was cut off.
        let mut data = good.clone();
        data.extend_from_slice(&(IDR_UNIT.len() as u32).to_be_bytes());
        data.extend_from_slice(&IDR_UNIT[..IDR_UNIT.len() - 1]);
        assert_eq!(walk(&data, Framing::Avcc), (vec![SPS_UNIT, PPS_UNIT], true));
        // A huge length (u32::MAX) must not overflow anything.
        let mut data = good.clone();
        data.extend_from_slice(&[0xff; 4]);
        data.extend_from_slice(IDR_UNIT);
        assert_eq!(walk(&data, Framing::Avcc), (vec![SPS_UNIT, PPS_UNIT], true));
        // A zero length ends it too, even with good units after it.
        let mut data = good.clone();
        data.extend_from_slice(&[0, 0, 0, 0]);
        data.extend(avcc(&[IDR_UNIT]));
        assert_eq!(walk(&data, Framing::Avcc), (vec![SPS_UNIT, PPS_UNIT], true));
        // Annex B said to be AVCC: `00 00 00 01` reads as a length of 1, then garbage.
        let data = annex_b(&[SPS_UNIT, PPS_UNIT], &[0, 0, 0, 1]);
        assert_eq!(walk(&data, Framing::Avcc), (vec![&SPS_UNIT[..1]], true));
        // Fused: done means done.
        let mut w = nals(&[0, 0, 0, 9, 1], Framing::Avcc);
        assert!(w.next().is_none() && w.next().is_none() && w.malformed());
    }

    #[test]
    fn round_trip_annex_b_to_avcc_and_back() {
        let units = [AUD_UNIT, SPS_UNIT, PPS_UNIT, SEI_UNIT, IDR_UNIT];
        let mut data = vec![0, 0];
        data.extend(annex_b(&units[..2], &[0, 0, 1]));
        data.extend(annex_b(&units[2..], &[0, 0, 0, 1]));
        data.extend_from_slice(&[0, 0]);

        let first: Vec<&[u8]> = nals(&data, Framing::AnnexB).map(|n| n.data).collect();
        let rebuilt = avcc(&first);
        let second: Vec<&[u8]> = nals(&rebuilt, Framing::Avcc).map(|n| n.data).collect();
        assert_eq!(first, units.to_vec());
        assert_eq!(first, second);
    }

    #[test]
    fn to_packetizer_avcc_becomes_annex_b_of_the_same_size() {
        let units = [SPS_UNIT, PPS_UNIT, IDR_UNIT];
        let data = avcc(&units);
        let (out, clean) = packetized(&data, Framing::Avcc);
        assert!(clean);
        assert_eq!(out.len(), data.len());
        assert_eq!(out, annex_b(&units, &START_CODE));

        // Malformed: what could be read, and false.
        let mut data = avcc(&[P_UNIT]);
        data.extend_from_slice(&[0, 0, 0x10, 0]);
        assert_eq!(packetized(&data, Framing::Avcc), (annex_b(&[P_UNIT], &START_CODE), false));
        assert_eq!(packetized(&[], Framing::Avcc), (vec![], true));

        // Units no Annex B can carry intact (a start code inside, a zero at the end): the rewrite
        // stops at them, with the units before them in `out`.
        let inside: &[u8] = &[0x41, 0x10, 0, 0, 1, 0x20];
        let zero_end: &[u8] = &[0x41, 0x10, 0];
        for bad in [inside, zero_end] {
            let data = avcc(&[SPS_UNIT, bad, PPS_UNIT]);
            assert_eq!(walk(&data, Framing::Avcc), (vec![SPS_UNIT, bad, PPS_UNIT], false));
            let before = annex_b(&[SPS_UNIT], &START_CODE);
            assert_eq!(packetized(&data, Framing::Avcc), (before, false));
        }
        // Zeros elsewhere are fine: emulation prevention, a zero at the start.
        let fine: &[u8] = &[0x41, 0, 0, 3, 0, 0, 3, 1, 0, 0x80];
        let data = avcc(&[fine, &[0, 0x65, 1]]);
        assert!(packetized(&data, Framing::Avcc).1);
    }

    #[test]
    fn to_packetizer_annex_b_is_copied_between_the_first_and_last_unit() {
        // Already what str0m wants: byte for byte.
        let data = annex_b(&[SPS_UNIT, PPS_UNIT, IDR_UNIT], &START_CODE);
        assert_eq!(packetized(&data, Framing::AnnexB), (data.clone(), true));

        // 3-byte start codes and zeros between units are kept as they are (str0m takes them);
        // leading zeros and the zeros after the last unit go.
        let mut inner = SPS_UNIT.to_vec();
        inner.extend_from_slice(&[0, 0, 1]);
        inner.extend_from_slice(PPS_UNIT);
        inner.extend_from_slice(&[0, 0, 0, 0, 1]);
        inner.extend_from_slice(P_UNIT);
        let mut data = vec![0, 0, 0, 0, 1];
        data.extend_from_slice(&inner);
        data.extend_from_slice(&[0, 0, 0]);
        let mut want = START_CODE.to_vec();
        want.extend_from_slice(&inner);
        assert_eq!(packetized(&data, Framing::AnnexB), (want.clone(), true));

        // Junk in front: left out, and reported.
        let mut junk = vec![0x55, 0, 0, 1];
        junk.extend_from_slice(&inner);
        assert_eq!(packetized(&junk, Framing::AnnexB), (want, false));

        // No start code: nothing to send (str0m itself would send it whole as one unit).
        assert_eq!(packetized(IDR_UNIT, Framing::AnnexB), (vec![], false));
        assert_eq!(packetized(&[0, 0, 0], Framing::AnnexB), (vec![], true));
    }

    #[test]
    fn keyframe_and_parameter_sets() {
        let key = [SPS_UNIT, PPS_UNIT, IDR_UNIT];
        let bare_key = [IDR_UNIT];
        let p = [P_UNIT];
        let only_sps = [SPS_UNIT, IDR_UNIT];
        let params = [SPS_UNIT, PPS_UNIT];
        for (units, keyframe, sets) in [
            (&key[..], true, true),
            (&bare_key[..], true, false),
            (&p[..], false, false),
            (&only_sps[..], true, false),
            (&params[..], false, true),
        ] {
            let with_start_codes = (annex_b(units, &[0, 0, 1]), Framing::AnnexB);
            for (data, framing) in [with_start_codes, (avcc(units), Framing::Avcc)] {
                assert_eq!(is_keyframe(&data, framing), keyframe, "{units:02x?} {framing:?}");
                assert_eq!(has_parameter_sets(&data, framing), sets, "{units:02x?} {framing:?}");
            }
        }
        assert!(!is_keyframe(&[], Framing::AnnexB) && !has_parameter_sets(&[], Framing::Avcc));
    }

    /// str0m 0.26's split of a written buffer into NAL units, transcribed from
    /// src/packet/h264.rs:113-126 (`next_ind`) and 237-268 (`packetize`), with `emit` skipping the
    /// empty ones (129-131). The type is not public without str0m's `unversioned` feature, so the
    /// real one is checked end to end in tests/h264_roundtrip.rs.
    fn str0m_split(payload: &[u8]) -> Vec<&[u8]> {
        fn next_ind(nalu: &[u8], start: usize) -> Option<(usize, usize)> {
            let mut zero_count = 0;
            for (i, &b) in nalu[start..].iter().enumerate() {
                if b == 0 {
                    zero_count += 1;
                    continue;
                } else if b == 1 && zero_count >= 2 {
                    return Some((start + i - zero_count, zero_count + 1));
                }
                zero_count = 0
            }
            None
        }
        let mut units = Vec::new();
        if payload.is_empty() {
            return units;
        }
        match next_ind(payload, 0) {
            None => units.push(payload),
            Some(mut ind) => loop {
                let prev = ind.0 + ind.1;
                match next_ind(payload, prev) {
                    Some(next) => {
                        units.push(&payload[prev..next.0]);
                        ind = next;
                    }
                    None => {
                        units.push(&payload[prev..]);
                        break;
                    }
                }
            },
        }
        units.retain(|u| !u.is_empty());
        units
    }

    /// The invariants, on one buffer: in bounds and never empty; Annex B units hold no start code
    /// and end in no zero; AVCC units account for every byte unless malformed; str0m splits what
    /// `to_packetizer` makes as `nals` does, into the units `nals` saw in the input (all of them
    /// when it says so, the ones before the damage when not).
    fn check(data: &[u8]) {
        for framing in [Framing::AnnexB, Framing::Avcc] {
            let (units, malformed) = walk(data, framing);
            let mut consumed = 0;
            for unit in &units {
                assert!(!unit.is_empty());
                let start = offset_in(data, unit);
                assert!(start + unit.len() <= data.len());
                consumed += 4 + unit.len();
                if framing == Framing::AnnexB {
                    assert!(start_code(unit).is_none(), "{data:02x?}");
                    assert_ne!(unit.last(), Some(&0), "{data:02x?}");
                }
            }
            if framing == Framing::Avcc {
                assert_eq!(consumed == data.len(), !malformed, "{data:02x?}");
            }
            let (out, clean) = packetized(data, framing);
            let sent = str0m_split(&out);
            let again = walk(&out, Framing::AnnexB);
            assert_eq!(again, (sent.clone(), false), "{framing:?} {data:02x?}");
            if clean {
                assert!(!malformed);
                assert_eq!(sent, units, "{framing:?} {data:02x?} -> {out:02x?}");
            } else {
                assert_eq!(sent[..], units[..sent.len()], "{framing:?} {data:02x?} -> {out:02x?}");
            }
            if framing == Framing::AnnexB || malformed {
                assert_eq!(clean, !malformed);
            }
            assert_eq!(is_keyframe(data, framing), units.iter().any(|u| u[0] & 0x1f == IDR));
        }
    }

    #[test]
    fn every_short_buffer() {
        // Every buffer up to 7 bytes over an alphabet that makes start codes, lengths and headers.
        const ALPHABET: [u8; 5] = [0x00, 0x01, 0x03, 0x65, 0xff];
        let (mut buf, mut count) = (Vec::new(), 0);
        for len in 0..=7u32 {
            for mut n in 0..ALPHABET.len().pow(len) {
                buf.clear();
                for _ in 0..len {
                    buf.push(ALPHABET[n % ALPHABET.len()]);
                    n /= ALPHABET.len();
                }
                check(&buf);
                count += 1;
            }
        }
        assert_eq!(count, (5usize.pow(8) - 1) / 4);
    }

    #[test]
    fn random_buffers() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut rand = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..3000 {
            // Mostly zeros and ones, so start codes and small lengths are everywhere.
            let len = (rand() % 300) as usize;
            let data: Vec<u8> = (0..len)
                .map(|_| match rand() % 8 {
                    0..=3 => 0,
                    4 => 1,
                    _ => rand() as u8,
                })
                .collect();
            check(&data);

            // Real AVCC, then cut at a random place or with one length byte changed.
            let count = 1 + rand() % 5;
            let units: Vec<Vec<u8>> = (0..count)
                .map(|_| (0..1 + rand() % 40).map(|_| rand() as u8 | 1).collect())
                .collect();
            let refs: Vec<&[u8]> = units.iter().map(|u| &u[..]).collect();
            let mut data = avcc(&refs);
            assert_eq!(walk(&data, Framing::Avcc), (refs.clone(), false));
            check(&data);
            if rand() % 2 == 0 {
                data.truncate((rand() as usize) % data.len());
            } else {
                let at = (rand() as usize) % data.len();
                data[at] = rand() as u8;
            }
            check(&data);
        }
    }
}
