//! THE PROFILE: what every connection of this library speaks, and nothing else.
//!   audio  Opus 48000/2, in-band FEC
//!   video  H.264 Constrained Baseline 3.1 (42e01f), packetization-mode 1, with RTX
//!   data   SCTP channels
//! One place builds the str0m configuration, from the numbers a negotiation fixed (sdp.rs writes
//! the same numbers into the description), so an SDP of ours can never say more.

use str0m::bwe::Bitrate;
use str0m::format::{Codec, CodecConfig, CodecSpec, FormatParams, PayloadParams};
use str0m::media::Frequency;
use str0m::rtp::Extension;
use str0m::{Rtc, RtcConfig};

use crate::sdp::{Ext, Numbers};

/// The payload types of the profile when this side offers (the numbers browsers use for the same
/// formats); an answer takes the offer's.
pub const H264_PT: u8 = 108;
pub const H264_RTX_PT: u8 = 109;
/// Constrained Baseline, level 3.1.
pub const H264_PROFILE_LEVEL_ID: u32 = 0x42e01f;

/// Where the bandwidth estimate starts before anything was measured.
pub const START_BITRATE: Bitrate = Bitrate::kbps(300);

/// The profile with the numbers this side offers.
pub fn config() -> RtcConfig {
    config_with(&Numbers::profile())
}

/// The profile with the numbers of a negotiation: ONE Opus, ONE H.264 variant (`enable_h264`
/// registers seven, and an offer that names them all then settles on another, seen with Chrome:
/// plain Baseline) and its RTX, the header extensions at the ids agreed. str0m fixes these at
/// build: the Direct API cannot renumber them later.
pub fn config_with(numbers: &Numbers) -> RtcConfig {
    let mut config = Rtc::builder()
        .clear_codecs()
        .clear_extension_map()
        .enable_bwe(Some(START_BITRATE))
        .set_stats_interval(Some(std::time::Duration::from_secs(1)));
    let mut opus = PayloadParams::new(
        numbers.opus.into(),
        None,
        CodecSpec {
            codec: Codec::Opus,
            clock_rate: Frequency::FORTY_EIGHT_KHZ,
            channels: Some(2),
            format: FormatParams { min_p_time: Some(10), use_inband_fec: Some(true), ..Default::default() },
        },
    );
    opus.set_fb_transport_cc(numbers.transport_cc[0]);
    let mut h264 = PayloadParams::new(
        numbers.h264.into(),
        numbers.h264_rtx.map(Into::into),
        CodecSpec {
            codec: Codec::H264,
            clock_rate: Frequency::NINETY_KHZ,
            channels: None,
            format: FormatParams {
                level_asymmetry_allowed: Some(true),
                packetization_mode: Some(1),
                profile_level_id: Some(H264_PROFILE_LEVEL_ID),
                ..Default::default()
            },
        },
    );
    h264.set_fb_transport_cc(numbers.transport_cc[1]);
    *config.codec_config() = CodecConfig::new_from_payload_params(vec![opus, h264]);
    for (id, ext) in &numbers.exts {
        config = config.set_extension(*id, extension(*ext));
    }
    config
}

fn extension(ext: Ext) -> Extension {
    match ext {
        Ext::AudioLevel => Extension::AudioLevel,
        Ext::AbsSendTime => Extension::AbsoluteSendTime,
        Ext::TransportCc => Extension::TransportSequenceNumber,
        Ext::Mid => Extension::RtpMid,
        Ext::VideoOrientation => Extension::VideoOrientation,
    }
}
