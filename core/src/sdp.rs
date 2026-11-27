//! The SDP of the profile, written and read by hand: std only, no parser library. It knows the one
//! profile (profile.rs) and nothing else, which is what makes it small: str0m's SDP module, with
//! the parser library under it, is not linked into the library at all (str0m's Direct API carries
//! what a description says; connection.rs makes those calls).
//!
//! WRITING: a description is BUNDLE of the sections of a `Session` in one transport — audio (Opus),
//! video (H.264 Constrained Baseline + its RTX), application (SCTP data channels) — with the ICE
//! credentials, the fingerprint, a=setup, rtcp-mux, the header extensions and the rtcp-fb of the
//! profile, a=msid and a=ssrc for what is sent, the local candidates in the first section.
//!
//! READING: `Remote::parse` keeps only what the profile needs and ignores every line it does not
//! know; it never panics, whatever the input (the tests feed it truncations and mutations).
//! Every value that later reaches str0m is checked here first: payload types in the dynamic
//! ranges and distinct, extension ids 1..=14 and one meaning each, mids of 1..=16 alphanumerics
//! and unique, SSRCs non-zero and unique, ICE credentials of ice-chars, a fingerprint str0m can
//! verify.
//!
//! NEGOTIATING:
//!   `answer_plan`  the answering side: the numbers are the offerer's (payload types, extension
//!                  ids, mids, the order and number of sections). The first audio section with
//!                  Opus and the first video section with the profile's H.264 are taken, with the
//!                  RTX of that H.264 if offered, and the first data section; every other section is
//!                  answered rejected (port 0). Nothing usable at all is an error.
//!   `read_answer`  the offering side: the answer must keep our sections and our numbers (str0m
//!                  cannot renumber after it is built); a section the answer rejects is not used.

use std::fmt::Write as _;

// ---------------------------------------------------------------------------------------------
// The profile's vocabulary.

/// The RTP header extensions of the profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ext {
    AudioLevel,
    AbsSendTime,
    TransportCc,
    Mid,
    VideoOrientation,
}

impl Ext {
    pub const ALL: [Ext; 5] = [Ext::AudioLevel, Ext::AbsSendTime, Ext::TransportCc, Ext::Mid, Ext::VideoOrientation];

    pub fn uri(self) -> &'static str {
        match self {
            Ext::AudioLevel => "urn:ietf:params:rtp-hdrext:ssrc-audio-level",
            Ext::AbsSendTime => "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time",
            Ext::TransportCc => "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
            Ext::Mid => "urn:ietf:params:rtp-hdrext:sdes:mid",
            Ext::VideoOrientation => "urn:3gpp:video-orientation",
        }
    }

    fn from_uri(uri: &str) -> Option<Ext> {
        Ext::ALL.into_iter().find(|e| e.uri() == uri)
    }

    /// Whether a section of this kind carries it (as str0m files them).
    fn applies(self, kind: Kind) -> bool {
        match self {
            Ext::AudioLevel => kind == Kind::Audio,
            Ext::VideoOrientation => kind == Kind::Video,
            _ => matches!(kind, Kind::Audio | Kind::Video),
        }
    }
}

/// The numbers one negotiation fixes, for the whole BUNDLE: payload types and extension ids. The
/// offering side's are `Numbers::profile()`; the answering side takes the offer's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Numbers {
    pub opus: u8,
    pub h264: u8,
    pub h264_rtx: Option<u8>,
    /// rtcp-fb transport-cc on [Opus, H.264].
    pub transport_cc: [bool; 2],
    /// (id, extension): ids 1..=14, each id and each extension once.
    pub exts: Vec<(u8, Ext)>,
}

impl Numbers {
    /// What this side offers: the numbers browsers use for the same formats.
    pub fn profile() -> Numbers {
        Numbers {
            opus: 111,
            h264: 108,
            h264_rtx: Some(109),
            transport_cc: [true, true],
            exts: vec![
                (1, Ext::AudioLevel),
                (2, Ext::AbsSendTime),
                (3, Ext::TransportCc),
                (4, Ext::Mid),
                (13, Ext::VideoOrientation),
            ],
        }
    }

    fn ext_id(&self, ext: Ext) -> Option<u8> {
        self.exts.iter().find(|(_, e)| *e == ext).map(|(id, _)| *id)
    }

    fn of_kind(&self, kind: Kind) -> Vec<(u8, Ext)> {
        self.exts.iter().copied().filter(|(_, e)| e.applies(kind)).collect()
    }
}

/// The profile's H.264: Constrained Baseline 3.1, packetization-mode 1.
pub const H264_FMTP: &str = "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f";
pub const OPUS_FMTP: &str = "minptime=10;useinbandfec=1";
/// The data line's SCTP port (str0m's, and every browser's).
pub const SCTP_PORT: u16 = 5000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Audio,
    Video,
    Application,
    /// A section the profile has no use for (it is answered rejected).
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    SendRecv,
    SendOnly,
    RecvOnly,
    Inactive,
}

impl Direction {
    pub fn sends(self) -> bool {
        matches!(self, Direction::SendRecv | Direction::SendOnly)
    }
    pub fn receives(self) -> bool {
        matches!(self, Direction::SendRecv | Direction::RecvOnly)
    }
    /// The direction that answers this one.
    fn answer(self) -> Direction {
        match self {
            Direction::SendRecv => Direction::SendRecv,
            Direction::SendOnly => Direction::RecvOnly,
            Direction::RecvOnly => Direction::SendOnly,
            Direction::Inactive => Direction::Inactive,
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Direction::SendRecv => "sendrecv",
            Direction::SendOnly => "sendonly",
            Direction::RecvOnly => "recvonly",
            Direction::Inactive => "inactive",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setup {
    ActPass,
    Active,
    Passive,
}

/// Why a description cannot be used (connection.rs turns every one into `Error::Sdp`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bad(pub &'static str);

// ---------------------------------------------------------------------------------------------
// Our side: what our descriptions say.

/// One section of our descriptions, in the order of the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub kind: Kind,
    pub mid: String,
    /// Rejected (port 0, outside BUNDLE): the offered m-line's media, proto and first format, as
    /// the answer repeats them.
    pub rejected: Option<(String, String, String)>,
    /// Ours, as written.
    pub direction: Direction,
    pub exts: Vec<(u8, Ext)>,
    /// What we send on it: the SSRC, and its RTX (video).
    pub ssrc: Option<(u32, Option<u32>)>,
    /// The msid track id.
    pub track: String,
    /// Video: the RTX payload type is part of it.
    pub rtx: bool,
}

/// The m-line of a section of ours the other side rejected, as later descriptions repeat it.
pub fn rejected_line(kind: Kind, numbers: &Numbers) -> (String, String, String) {
    let rtp = |media: &str, pt: u8| (media.to_owned(), "UDP/TLS/RTP/SAVPF".to_owned(), pt.to_string());
    match kind {
        Kind::Audio => rtp("audio", numbers.opus),
        Kind::Video => rtp("video", numbers.h264),
        _ => ("application".into(), "UDP/DTLS/SCTP".into(), "webrtc-datachannel".into()),
    }
}

/// What every description of one connection repeats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// The o= line: the id once, the version one more per description.
    pub id: u64,
    pub version: u64,
    pub numbers: Numbers,
    pub sections: Vec<Section>,
    /// The msid stream of all our tracks, and the RTCP CNAME.
    pub stream: String,
    pub cname: String,
    /// What our SCTP takes (a=max-message-size).
    pub max_message_size: u32,
}

/// The transport part of a description we write.
pub struct Transport<'a> {
    pub ufrag: &'a str,
    pub pwd: &'a str,
    /// "sha-256 AB:CD:…" (str0m's `Fingerprint` display).
    pub fingerprint: &'a str,
    pub setup: Setup,
    /// `candidate:` lines.
    pub candidates: &'a [String],
}

impl Session {
    /// The first offer: audio, video and the data line, sendrecv, the profile's numbers. The
    /// SSRCs are filled in by the caller.
    pub fn offer(id: u64, stream: String, cname: String, tracks: [String; 2], max_message_size: u32) -> Session {
        let numbers = Numbers::profile();
        let [audio_track, video_track] = tracks;
        let media = |kind: Kind, mid: &str, track: String| Section {
            kind,
            mid: mid.into(),
            rejected: None,
            direction: Direction::SendRecv,
            exts: numbers.of_kind(kind),
            ssrc: None,
            track,
            rtx: kind == Kind::Video && numbers.h264_rtx.is_some(),
        };
        let sections = vec![
            media(Kind::Audio, "0", audio_track),
            media(Kind::Video, "1", video_track),
            Section {
                kind: Kind::Application,
                mid: "2".into(),
                rejected: None,
                direction: Direction::SendRecv,
                exts: Vec::new(),
                ssrc: None,
                track: String::new(),
                rtx: false,
            },
        ];
        Session { id, version: 1, numbers, sections, stream, cname, max_message_size }
    }

    /// The section of a kind that is in use.
    pub fn section(&self, kind: Kind) -> Option<&Section> {
        self.sections.iter().find(|s| s.kind == kind && s.rejected.is_none())
    }

    pub fn section_mut(&mut self, kind: Kind) -> Option<&mut Section> {
        self.sections.iter_mut().find(|s| s.kind == kind && s.rejected.is_none())
    }

    /// The description text. Every description is complete (all sections, the transport in each),
    /// so a restart offer or an answer to one says everything again.
    pub fn write(&mut self, transport: &Transport) -> String {
        self.version += 1;
        let mut out = String::with_capacity(2048);
        let mut line = |s: &str| {
            out.push_str(s);
            out.push_str("\r\n");
        };
        line("v=0");
        line(&format!("o=tinyrtc {} {} IN IP4 0.0.0.0", self.id, self.version));
        line("s=-");
        line("t=0 0");
        let bundle: Vec<&str> = self.sections.iter().filter(|s| s.rejected.is_none()).map(|s| s.mid.as_str()).collect();
        if !bundle.is_empty() {
            line(&format!("a=group:BUNDLE {}", bundle.join(" ")));
        }
        line("a=extmap-allow-mixed");
        line(&format!("a=msid-semantic: WMS {}", self.stream));
        let first = self.sections.iter().position(|s| s.rejected.is_none());
        for (index, section) in self.sections.iter().enumerate() {
            if let Some((media, proto, fmt)) = &section.rejected {
                line(&format!("m={media} 0 {proto} {fmt}"));
                line("c=IN IP4 0.0.0.0");
                if !section.mid.is_empty() {
                    line(&format!("a=mid:{}", section.mid));
                }
                continue;
            }
            let n = &self.numbers;
            let formats = match section.kind {
                Kind::Audio => n.opus.to_string(),
                Kind::Video => match n.h264_rtx.filter(|_| section.rtx) {
                    Some(rtx) => format!("{} {rtx}", n.h264),
                    None => n.h264.to_string(),
                },
                _ => String::new(),
            };
            match section.kind {
                Kind::Application => line("m=application 9 UDP/DTLS/SCTP webrtc-datachannel"),
                Kind::Audio => line(&format!("m=audio 9 UDP/TLS/RTP/SAVPF {formats}")),
                Kind::Video => line(&format!("m=video 9 UDP/TLS/RTP/SAVPF {formats}")),
                Kind::Other => continue,
            }
            line("c=IN IP4 0.0.0.0");
            if Some(index) == first {
                for candidate in transport.candidates {
                    line(&format!("a={candidate}"));
                }
            }
            line(&format!("a=ice-ufrag:{}", transport.ufrag));
            line(&format!("a=ice-pwd:{}", transport.pwd));
            line("a=ice-options:trickle");
            line(&format!("a=fingerprint:{}", transport.fingerprint));
            line(match transport.setup {
                Setup::ActPass => "a=setup:actpass",
                Setup::Active => "a=setup:active",
                Setup::Passive => "a=setup:passive",
            });
            line(&format!("a=mid:{}", section.mid));
            if section.kind == Kind::Application {
                line(&format!("a=sctp-port:{SCTP_PORT}"));
                line(&format!("a=max-message-size:{}", self.max_message_size));
                continue;
            }
            for (id, ext) in &section.exts {
                line(&format!("a=extmap:{id} {}", ext.uri()));
            }
            line(&format!("a={}", section.direction.as_str()));
            line(&format!("a=msid:{} {}", self.stream, section.track));
            line("a=rtcp-mux");
            if section.kind == Kind::Audio {
                let pt = n.opus;
                line(&format!("a=rtpmap:{pt} opus/48000/2"));
                if n.transport_cc[0] {
                    line(&format!("a=rtcp-fb:{pt} transport-cc"));
                }
                line(&format!("a=fmtp:{pt} {OPUS_FMTP}"));
            } else {
                let pt = n.h264;
                line(&format!("a=rtpmap:{pt} H264/90000"));
                if n.transport_cc[1] {
                    line(&format!("a=rtcp-fb:{pt} transport-cc"));
                }
                // The feedback str0m's H.264 parameters have (format/payload_params.rs).
                for fb in ["goog-remb", "ccm fir", "nack", "nack pli"] {
                    line(&format!("a=rtcp-fb:{pt} {fb}"));
                }
                line(&format!("a=fmtp:{pt} {H264_FMTP}"));
                if let Some(rtx) = n.h264_rtx.filter(|_| section.rtx) {
                    line(&format!("a=rtpmap:{rtx} rtx/90000"));
                    line(&format!("a=fmtp:{rtx} apt={pt}"));
                }
            }
            if let (Some((ssrc, rtx)), true) = (section.ssrc, section.direction.sends()) {
                if let Some(rtx) = rtx {
                    line(&format!("a=ssrc-group:FID {ssrc} {rtx}"));
                }
                for s in std::iter::once(ssrc).chain(rtx) {
                    line(&format!("a=ssrc:{s} cname:{}", self.cname));
                    line(&format!("a=ssrc:{s} msid:{} {}", self.stream, section.track));
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Their side: what a description from the other side says.

/// The transport attributes, at session level or in a section.
#[derive(Debug, Clone, Default)]
struct Attrs {
    ufrag: Option<String>,
    pwd: Option<String>,
    /// (hash function, digest).
    fingerprints: Vec<(String, Vec<u8>)>,
    setup: Option<Setup>,
    candidates: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RtpMap {
    pub pt: u8,
    pub name: String,
    pub clock: u32,
    pub channels: Option<u8>,
}

/// One m= section of the other side's description.
#[derive(Debug, Clone)]
pub struct RemoteSection {
    pub media: String,
    pub port: u16,
    pub proto: String,
    pub formats: Vec<String>,
    pub mid: Option<String>,
    pub direction: Direction,
    pub bundle_only: bool,
    pub rtpmaps: Vec<RtpMap>,
    pub fmtps: Vec<(u8, String)>,
    /// (payload type, feedback): `*` is kept as None.
    pub rtcp_fbs: Vec<(Option<u8>, String)>,
    pub extmaps: Vec<(u8, String)>,
    /// SSRCs in the order of their first a=ssrc line.
    pub ssrcs: Vec<u32>,
    /// a=ssrc-group:FID main rtx.
    pub fids: Vec<(u32, u32)>,
    attrs: Attrs,
}

impl RemoteSection {
    fn new(media: &str, port: u16, proto: &str, formats: Vec<String>) -> RemoteSection {
        RemoteSection {
            media: media.into(),
            port,
            proto: proto.into(),
            formats,
            mid: None,
            direction: Direction::SendRecv,
            bundle_only: false,
            rtpmaps: Vec::new(),
            fmtps: Vec::new(),
            rtcp_fbs: Vec::new(),
            extmaps: Vec::new(),
            ssrcs: Vec::new(),
            fids: Vec::new(),
            attrs: Attrs::default(),
        }
    }

    fn kind(&self) -> Kind {
        match self.media.as_str() {
            "audio" if self.proto.contains("RTP/SAVPF") => Kind::Audio,
            "video" if self.proto.contains("RTP/SAVPF") => Kind::Video,
            "application" if self.proto.contains("DTLS/SCTP") => Kind::Application,
            _ => Kind::Other,
        }
    }

    /// The payload types of the m= line, in its order (the sender's preference).
    fn pts(&self) -> impl Iterator<Item = u8> + '_ {
        self.formats.iter().filter_map(|f| f.parse::<u8>().ok()).filter(|pt| *pt <= 127)
    }

    fn rtpmap(&self, pt: u8) -> Option<&RtpMap> {
        self.rtpmaps.iter().find(|r| r.pt == pt)
    }

    fn fmtp(&self, pt: u8) -> Option<&str> {
        self.fmtps.iter().find(|(p, _)| *p == pt).map(|(_, v)| v.as_str())
    }

    fn has_fb(&self, pt: u8, fb: &str) -> bool {
        self.rtcp_fbs.iter().any(|(p, f)| p.is_none_or(|p| p == pt) && f == fb)
    }

    /// Rejected by its sender: port 0, unless it is bundle-only in the group.
    fn is_rejected(&self, bundle: &[String]) -> bool {
        self.port == 0 && !(self.bundle_only && self.mid.as_ref().is_some_and(|m| bundle.contains(m)))
    }

    /// Opus, stereo at 48 kHz: its payload type, the first offered.
    fn opus(&self) -> Option<u8> {
        self.pts().find(|pt| {
            self.rtpmap(*pt).is_some_and(|r| r.name.eq_ignore_ascii_case("opus") && r.clock == 48_000 && r.channels == Some(2))
        })
    }

    /// The profile's H.264 (Constrained Baseline, packetization-mode 1, any level), the first
    /// offered, and its RTX.
    fn h264(&self) -> Option<(u8, Option<u8>)> {
        let pt = self.pts().find(|pt| {
            self.rtpmap(*pt).is_some_and(|r| r.name.eq_ignore_ascii_case("h264") && r.clock == 90_000)
                && self.fmtp(*pt).is_some_and(is_profile_h264)
        })?;
        let rtx = self.pts().find(|rtx| {
            self.rtpmap(*rtx).is_some_and(|r| r.name.eq_ignore_ascii_case("rtx") && r.clock == 90_000)
                && self.fmtp(*rtx).and_then(|f| fmtp_value(f, "apt")).and_then(|v| v.parse::<u8>().ok()) == Some(pt)
        });
        Some((pt, rtx))
    }

    /// What it sends, as far as its a=ssrc lines say: the first SSRC that repairs nothing, and
    /// the RTX of it.
    fn sent_ssrc(&self) -> Option<(u32, Option<u32>)> {
        let repairs = |s: u32| self.fids.iter().any(|(_, rtx)| *rtx == s);
        let main = self.ssrcs.iter().copied().find(|s| *s != 0 && !repairs(*s))?;
        let rtx = self.fids.iter().find(|(m, r)| *m == main && *r != 0 && *r != main).map(|(_, r)| *r);
        Some((main, rtx))
    }

    /// The extensions of the profile it lists, with valid ids.
    fn profile_exts(&self) -> Vec<(u8, Ext)> {
        let mut out: Vec<(u8, Ext)> = Vec::new();
        for (id, uri) in &self.extmaps {
            let Some(ext) = Ext::from_uri(uri) else { continue };
            if !(1..=14).contains(id) || out.iter().any(|(i, e)| i == id || *e == ext) {
                continue;
            }
            out.push((*id, ext));
        }
        out
    }
}

/// The other side's description.
#[derive(Debug, Clone, Default)]
pub struct Remote {
    pub ice_lite: bool,
    pub bundle: Option<Vec<String>>,
    pub sections: Vec<RemoteSection>,
    attrs: Attrs,
}

/// The other side's transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTransport {
    pub ufrag: String,
    pub pwd: String,
    pub fingerprint: (String, Vec<u8>),
    pub setup: Option<Setup>,
    pub ice_lite: bool,
}

impl Remote {
    /// Reads a description. Fails only when there is no m= section at all (that is not a
    /// description); everything else is judged by the negotiation.
    pub fn parse(text: &str) -> Result<Remote, Bad> {
        let mut remote = Remote::default();
        for raw in text.split('\n') {
            let line = raw.trim();
            let Some((kind, value)) = line.split_once('=') else { continue };
            match kind {
                "m" => {
                    let mut words = value.split_ascii_whitespace();
                    let media = words.next().unwrap_or("");
                    // "9" or "9/2" (a port count); an unreadable port is a rejected section.
                    let port = words.next().and_then(|p| p.split('/').next()).and_then(|p| p.parse().ok()).unwrap_or(0);
                    let proto = words.next().unwrap_or("");
                    let formats = words.map(str::to_owned).collect();
                    remote.sections.push(RemoteSection::new(media, port, proto, formats));
                }
                "a" => {
                    let (name, arg) = value.split_once(':').unwrap_or((value, ""));
                    match remote.sections.last_mut() {
                        Some(section) => section_attribute(section, name, arg.trim()),
                        None => session_attribute(&mut remote, name, arg.trim()),
                    }
                }
                _ => {}
            }
        }
        if remote.sections.is_empty() {
            return Err(Bad("no m= section"));
        }
        Ok(remote)
    }

    fn bundle_mids(&self) -> &[String] {
        self.bundle.as_deref().unwrap_or(&[])
    }

    /// A mid names one section (str0m refuses a description where it does not).
    fn mids_unique(&self) -> Result<(), Bad> {
        let mids: Vec<&String> = self.sections.iter().filter_map(|s| s.mid.as_ref()).collect();
        match mids.iter().enumerate().any(|(i, m)| mids[..i].contains(m)) {
            true => Err(Bad("a mid names two sections")),
            false => Ok(()),
        }
    }

    /// The transport, as str0m reads it: from the section the BUNDLE group is tagged with (its
    /// first mid), else from the session level.
    pub fn transport(&self) -> Result<RemoteTransport, Bad> {
        let tag = self.bundle_mids().first();
        let tagged = self.sections.iter().find(|s| tag.is_some() && s.mid.as_ref() == tag).map(|s| &s.attrs);
        // Each field from the tagged section, else from the session level.
        let ufrag = tagged.and_then(|a| a.ufrag.clone()).or_else(|| self.attrs.ufrag.clone());
        let pwd = tagged.and_then(|a| a.pwd.clone()).or_else(|| self.attrs.pwd.clone());
        let (Some(ufrag), Some(pwd)) = (ufrag, pwd) else { return Err(Bad("no a=ice-ufrag / a=ice-pwd")) };
        if !ice_chars(&ufrag, 4) || !ice_chars(&pwd, 22) {
            return Err(Bad("ICE credentials out of their grammar"));
        }
        let fingerprints = match tagged.filter(|a| !a.fingerprints.is_empty()) {
            Some(a) => &a.fingerprints,
            None => &self.attrs.fingerprints,
        };
        // sha-256 before sha-1 (RFC 8122 §5.1); nothing else can be verified.
        let pick = |hash: &str, len: usize| {
            fingerprints.iter().find(|(h, bytes)| h.eq_ignore_ascii_case(hash) && bytes.len() == len)
        };
        let fingerprint = pick("sha-256", 32).or_else(|| pick("sha-1", 20)).cloned().ok_or(Bad("no usable a=fingerprint"))?;
        let setup = tagged.and_then(|a| a.setup).or(self.attrs.setup);
        Ok(RemoteTransport { ufrag, pwd, fingerprint, setup, ice_lite: self.ice_lite })
    }

    /// Every candidate it carries (as str0m takes them: from every section and the session),
    /// once each.
    pub fn candidates(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for c in self.attrs.candidates.iter().chain(self.sections.iter().flat_map(|s| s.attrs.candidates.iter())) {
            if !out.contains(c) {
                out.push(c.clone());
            }
        }
        out
    }
}

fn session_attribute(remote: &mut Remote, name: &str, arg: &str) {
    match name {
        "group" => {
            let mut words = arg.split_ascii_whitespace();
            if words.next() == Some("BUNDLE") && remote.bundle.is_none() {
                remote.bundle = Some(words.map(str::to_owned).collect());
            }
        }
        "ice-lite" => remote.ice_lite = true,
        _ => transport_attribute(&mut remote.attrs, name, arg),
    }
}

fn transport_attribute(attrs: &mut Attrs, name: &str, arg: &str) {
    match name {
        "ice-ufrag" if attrs.ufrag.is_none() => attrs.ufrag = Some(arg.to_owned()),
        "ice-pwd" if attrs.pwd.is_none() => attrs.pwd = Some(arg.to_owned()),
        "fingerprint" => {
            if let Some(fingerprint) = parse_fingerprint(arg) {
                attrs.fingerprints.push(fingerprint);
            }
        }
        "setup" if attrs.setup.is_none() => {
            attrs.setup = match arg {
                "actpass" => Some(Setup::ActPass),
                "active" => Some(Setup::Active),
                "passive" => Some(Setup::Passive),
                _ => None,
            }
        }
        "candidate" => attrs.candidates.push(format!("candidate:{arg}")),
        _ => {}
    }
}

fn section_attribute(section: &mut RemoteSection, name: &str, arg: &str) {
    let pt_and = |arg: &str| -> Option<(u8, String)> {
        let (pt, rest) = arg.split_once(|c: char| c.is_ascii_whitespace())?;
        let pt = pt.parse::<u8>().ok().filter(|pt| *pt <= 127)?;
        Some((pt, rest.trim().to_owned()))
    };
    match name {
        "mid" if section.mid.is_none() => section.mid = Some(arg.to_owned()),
        "sendrecv" => section.direction = Direction::SendRecv,
        "sendonly" => section.direction = Direction::SendOnly,
        "recvonly" => section.direction = Direction::RecvOnly,
        "inactive" => section.direction = Direction::Inactive,
        "bundle-only" => section.bundle_only = true,
        "rtpmap" => {
            let Some((pt, value)) = pt_and(arg) else { return };
            let mut parts = value.split('/');
            let name = parts.next().unwrap_or("").to_owned();
            let Some(clock) = parts.next().and_then(|c| c.trim().parse::<u32>().ok()) else { return };
            let channels = match parts.next() {
                Some(c) => match c.trim().parse::<u8>() {
                    Ok(c) => Some(c),
                    Err(_) => return,
                },
                None => None,
            };
            if section.rtpmap(pt).is_none() {
                section.rtpmaps.push(RtpMap { pt, name, clock, channels });
            }
        }
        "fmtp" => {
            if let Some((pt, value)) = pt_and(arg) {
                if section.fmtp(pt).is_none() {
                    section.fmtps.push((pt, value));
                }
            }
        }
        "rtcp-fb" => {
            let (pt, fb) = arg.split_once(|c: char| c.is_ascii_whitespace()).unwrap_or((arg, ""));
            let pt = match pt {
                "*" => None,
                pt => match pt.parse::<u8>() {
                    Ok(pt) if pt <= 127 => Some(pt),
                    _ => return,
                },
            };
            section.rtcp_fbs.push((pt, fb.split_ascii_whitespace().collect::<Vec<_>>().join(" ")));
        }
        "extmap" => {
            let mut words = arg.split_ascii_whitespace();
            // "3" or "3/sendonly".
            let Some(id) = words.next().and_then(|w| w.split('/').next()).and_then(|id| id.parse::<u8>().ok()) else {
                return;
            };
            let Some(uri) = words.next() else { return };
            section.extmaps.push((id, uri.to_owned()));
        }
        "ssrc" => {
            let ssrc = arg.split(|c: char| c.is_ascii_whitespace() || c == ':').next().and_then(|s| s.parse::<u32>().ok());
            if let Some(ssrc) = ssrc {
                if !section.ssrcs.contains(&ssrc) {
                    section.ssrcs.push(ssrc);
                }
            }
        }
        "ssrc-group" => {
            let mut words = arg.split_ascii_whitespace();
            if words.next() == Some("FID") {
                let mut ssrc = || words.next().and_then(|s| s.parse::<u32>().ok());
                if let (Some(main), Some(rtx)) = (ssrc(), ssrc()) {
                    section.fids.push((main, rtx));
                }
            }
        }
        _ => transport_attribute(&mut section.attrs, name, arg),
    }
}

/// "sha-256 AB:CD:…" → ("sha-256", bytes).
fn parse_fingerprint(arg: &str) -> Option<(String, Vec<u8>)> {
    let (hash, hex) = arg.split_once(|c: char| c.is_ascii_whitespace())?;
    let bytes: Option<Vec<u8>> = hex
        .trim()
        .split(':')
        .map(|b| if b.len() == 2 { u8::from_str_radix(b, 16).ok() } else { None })
        .collect();
    Some((hash.to_owned(), bytes?))
}

/// ice-char = ALPHA / DIGIT / "+" / "/", min..=256 of them (RFC 8839 §5.4).
fn ice_chars(s: &str, min: usize) -> bool {
    (min..=256).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

/// A mid str0m keeps exactly: 1..=16 alphanumerics (its `Mid` is 16 bytes and maps anything else
/// to '_').
fn usable_mid(mid: &str) -> bool {
    (1..=16).contains(&mid.len()) && mid.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// A payload type str0m can take for a dynamic format: not a static one, not one RTCP's packet
/// types overlap (64..=95).
fn dynamic_pt(pt: u8) -> bool {
    (35..=63).contains(&pt) || (96..=127).contains(&pt)
}

fn fmtp_value<'a>(fmtp: &'a str, key: &str) -> Option<&'a str> {
    fmtp.split(';').filter_map(|kv| kv.split_once('=')).find(|(k, _)| k.trim().eq_ignore_ascii_case(key)).map(|(_, v)| v.trim())
}

/// packetization-mode=1 and a profile-level-id of Constrained Baseline (RFC 6184, the table
/// libwebrtc and str0m use: 42 x1xx0000, 4D 1xxx0000, 58 11xx0000).
fn is_profile_h264(fmtp: &str) -> bool {
    if fmtp_value(fmtp, "packetization-mode") != Some("1") {
        return false;
    }
    let Some(id) = fmtp_value(fmtp, "profile-level-id") else { return false };
    if id.len() != 6 {
        return false;
    }
    let Ok(id) = u32::from_str_radix(id, 16) else { return false };
    let (idc, iop) = ((id >> 16) as u8, (id >> 8) as u8);
    match idc {
        0x42 => iop & 0x4f == 0x40,
        0x4d => iop & 0x8f == 0x80,
        0x58 => iop & 0xcf == 0xc0,
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------------
// Negotiation.

/// What the other side sends on a section, and whether it receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer {
    pub sends: bool,
    pub receives: bool,
    /// Its SSRC and RTX, when its description names them.
    pub ssrc: Option<(u32, Option<u32>)>,
}

/// The answering side's plan: the numbers, our sections (SSRCs to be filled in), what the other
/// side does on each section (None = not in use).
#[derive(Debug, Clone)]
pub struct Plan {
    pub numbers: Numbers,
    pub sections: Vec<Section>,
    pub peers: Vec<Option<Peer>>,
    pub transport: RemoteTransport,
    pub candidates: Vec<String>,
}

/// Plans the answer to an offer (see the module comment).
pub fn answer_plan(offer: &Remote) -> Result<Plan, Bad> {
    let bundle = offer.bundle.as_ref().ok_or(Bad("no a=group:BUNDLE"))?;
    offer.mids_unique()?;
    let transport = offer.transport()?;
    let mut taken = [false; 3]; // audio, video, application
    let mut opus: Option<(u8, bool)> = None;
    let mut h264: Option<(u8, Option<u8>, bool)> = None;
    let mut exts: Vec<(u8, Ext)> = Vec::new();
    let mut mids: Vec<String> = Vec::new();
    let mut ssrcs: Vec<u32> = Vec::new();
    let mut sections = Vec::with_capacity(offer.sections.len());
    let mut peers = Vec::with_capacity(offer.sections.len());

    for s in &offer.sections {
        // The answer repeats a rejected section's m-line and mid: only as visible ASCII.
        let token = |t: &str, otherwise: &str| -> String {
            if !t.is_empty() && t.bytes().all(|b| b.is_ascii_graphic()) { t.to_owned() } else { otherwise.to_owned() }
        };
        let rejected = || Section {
            kind: Kind::Other,
            mid: token(s.mid.as_deref().unwrap_or(""), ""),
            rejected: Some((
                token(&s.media, "application"),
                token(&s.proto, "UDP/TLS/RTP/SAVPF"),
                token(s.formats.first().map(String::as_str).unwrap_or(""), "0"),
            )),
            direction: Direction::Inactive,
            exts: Vec::new(),
            ssrc: None,
            track: String::new(),
            rtx: false,
        };
        let kind = s.kind();
        let slot = match kind {
            Kind::Audio => 0,
            Kind::Video => 1,
            Kind::Application => 2,
            Kind::Other => 3,
        };
        let mid = s.mid.as_deref().filter(|m| usable_mid(m) && bundle.iter().any(|b| b == m));
        let usable = slot < 3 && !taken[slot] && !s.is_rejected(bundle) && mid.is_some_and(|m| !mids.iter().any(|t| t == m));
        // The codec of the section, consistent with what is taken already.
        let codec_ok = match kind {
            Kind::Audio => s.opus().filter(|pt| dynamic_pt(*pt) && h264.is_none_or(|(h, r, _)| *pt != h && Some(*pt) != r)),
            Kind::Video => s
                .h264()
                .filter(|(pt, rtx)| dynamic_pt(*pt) && rtx.is_none_or(dynamic_pt) && opus.is_none_or(|(o, _)| o != *pt && Some(o) != *rtx))
                .map(|(pt, _)| pt),
            Kind::Application => Some(0),
            Kind::Other => None,
        };
        let (true, Some(_), Some(mid)) = (usable, codec_ok, mid) else {
            sections.push(rejected());
            peers.push(None);
            continue;
        };
        taken[slot] = true;
        mids.push(mid.to_owned());
        if kind == Kind::Application {
            sections.push(Section {
                kind,
                mid: mid.to_owned(),
                rejected: None,
                direction: Direction::SendRecv,
                exts: Vec::new(),
                ssrc: None,
                track: String::new(),
                rtx: false,
            });
            peers.push(Some(Peer { sends: true, receives: true, ssrc: None }));
            continue;
        }
        let mut rtx = false;
        match kind {
            Kind::Audio => {
                let pt = s.opus().unwrap_or(0);
                opus = Some((pt, s.has_fb(pt, "transport-cc")));
            }
            _ => {
                let (pt, r) = s.h264().unwrap_or((0, None));
                rtx = r.is_some();
                h264 = Some((pt, r, s.has_fb(pt, "transport-cc")));
            }
        }
        // Extensions: one meaning per id and one id per extension across the BUNDLE; what
        // conflicts with an earlier section is left out of this one.
        let mut own = Vec::new();
        for (id, ext) in s.profile_exts() {
            if !ext.applies(kind) {
                continue;
            }
            match exts.iter().find(|(i, e)| *i == id || *e == ext) {
                Some(known) if *known == (id, ext) => own.push((id, ext)),
                Some(_) => {}
                None => {
                    exts.push((id, ext));
                    own.push((id, ext));
                }
            }
        }
        // Its SSRCs, once in the whole description.
        let ssrc = s.sent_ssrc().filter(|(main, rtx)| {
            !ssrcs.contains(main) && rtx.is_none_or(|r| !ssrcs.contains(&r))
        });
        if let Some((main, rtx)) = ssrc {
            ssrcs.push(main);
            ssrcs.extend(rtx);
        }
        let direction = s.direction.answer();
        sections.push(Section {
            kind,
            mid: mid.to_owned(),
            rejected: None,
            direction,
            exts: own,
            ssrc: None,
            track: String::new(),
            rtx,
        });
        peers.push(Some(Peer { sends: s.direction.sends(), receives: s.direction.receives(), ssrc: ssrc.filter(|_| s.direction.sends()) }));
    }

    if !taken.iter().any(|t| *t) {
        return Err(Bad("nothing in the offer the profile can take"));
    }
    // A kind the offer does not have is answered rejected, so its numbers are never used: they
    // only have to be valid and distinct from the others.
    let (opus_pt, opus_twcc) = opus.unwrap_or((spare_pt(&[h264.map(|h| h.0), h264.and_then(|h| h.1)]), false));
    let (h264_pt, h264_rtx, h264_twcc) = h264.unwrap_or((spare_pt(&[Some(opus_pt)]), None, false));
    let numbers = Numbers { opus: opus_pt, h264: h264_pt, h264_rtx, transport_cc: [opus_twcc, h264_twcc], exts };
    Ok(Plan { numbers, sections, peers, transport, candidates: offer.candidates() })
}

/// A dynamic payload type none of `used` is.
fn spare_pt(used: &[Option<u8>]) -> u8 {
    (96..=127).find(|pt| !used.contains(&Some(*pt))).unwrap_or(96)
}

/// What an answer to our offer agreed to.
#[derive(Debug, Clone)]
pub struct Answer {
    /// Per section of ours: what the other side does (None = rejected by it).
    pub peers: Vec<Option<Peer>>,
    /// The other side takes the RTX of the video.
    pub video_rtx: bool,
    /// The other side gives transport-cc feedback.
    pub transport_cc: bool,
    pub transport: RemoteTransport,
    pub candidates: Vec<String>,
}

/// Reads the answer to an offer of `ours` (see the module comment).
pub fn read_answer(ours: &Session, answer: &Remote) -> Result<Answer, Bad> {
    let bundle = answer.bundle.as_ref().ok_or(Bad("no a=group:BUNDLE"))?;
    answer.mids_unique()?;
    let transport = answer.transport()?;
    if answer.sections.len() != ours.sections.len() {
        return Err(Bad("the answer's sections are not the offer's"));
    }
    let n = &ours.numbers;
    let mut peers = Vec::with_capacity(ours.sections.len());
    let mut video_rtx = false;
    let mut transport_cc = false;
    let mut ssrcs: Vec<u32> = Vec::new();
    for (mine, theirs) in ours.sections.iter().zip(&answer.sections) {
        if mine.rejected.is_some() {
            peers.push(None);
            continue;
        }
        if theirs.kind() != mine.kind || theirs.mid.as_ref().is_some_and(|m| *m != mine.mid) {
            return Err(Bad("an answered section is not the offered one"));
        }
        let in_bundle = theirs.mid.as_ref().is_some_and(|m| bundle.contains(m));
        if theirs.is_rejected(bundle) || !in_bundle {
            peers.push(None);
            continue;
        }
        // Our extension ids are fixed: an answer may leave one out, never renumber it.
        for (id, uri) in &theirs.extmaps {
            if let Some(ext) = Ext::from_uri(uri) {
                if n.ext_id(ext).is_some_and(|ours| ours != *id) {
                    return Err(Bad("the answer renumbers an extension"));
                }
            }
        }
        let codec = match mine.kind {
            Kind::Audio => answered(theirs, n.opus, "opus", 48_000)?,
            Kind::Video => answered(theirs, n.h264, "h264", 90_000)?,
            _ => true,
        };
        if !codec {
            peers.push(None);
            continue;
        }
        match mine.kind {
            Kind::Audio => transport_cc |= theirs.has_fb(n.opus, "transport-cc"),
            Kind::Video => {
                transport_cc |= theirs.has_fb(n.h264, "transport-cc");
                video_rtx = n.h264_rtx.is_some_and(|rtx| answered(theirs, rtx, "rtx", 90_000) == Ok(true));
            }
            _ => {}
        }
        let ssrc = theirs.sent_ssrc().filter(|(main, rtx)| !ssrcs.contains(main) && rtx.is_none_or(|r| !ssrcs.contains(&r)));
        if let Some((main, rtx)) = ssrc {
            ssrcs.push(main);
            ssrcs.extend(rtx);
        }
        let direction = theirs.direction;
        peers.push(Some(Peer { sends: direction.sends(), receives: direction.receives(), ssrc: ssrc.filter(|_| direction.sends()) }));
    }
    if peers.iter().all(Option::is_none) {
        return Err(Bad("the answer takes nothing"));
    }
    Ok(Answer { peers, video_rtx, transport_cc, transport, candidates: answer.candidates() })
}

/// Whether the answered section has our payload type `pt` as `codec`: an answer that names the
/// codec under another number is an error (str0m cannot follow it).
fn answered(section: &RemoteSection, pt: u8, codec: &str, clock: u32) -> Result<bool, Bad> {
    let named = |r: &RtpMap| r.name.eq_ignore_ascii_case(codec) && r.clock == clock;
    if section.pts().any(|p| p == pt) {
        return match section.rtpmap(pt) {
            Some(r) if named(r) => Ok(true),
            _ => Err(Bad("the answer maps our payload type to another codec")),
        };
    }
    if section.rtpmaps.iter().any(|r| named(r) && section.pts().any(|p| p == r.pt)) {
        return Err(Bad("the answer renumbers a codec"));
    }
    Ok(false)
}

/// A random-enough number for ids (session id, msid tokens) without a dependency: the hasher
/// std seeds per process, mixed with a counter and the clock.
pub fn random_u64() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    if let Ok(t) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        h.write_u128(t.as_nanos());
    }
    h.finish()
}

/// `len` alphanumerics (msid stream and track ids, the CNAME).
pub fn random_token(len: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = String::with_capacity(len);
    let mut bits = 0u64;
    for i in 0..len {
        if i % 8 == 0 {
            bits = random_u64();
        }
        let _ = out.write_char(ALPHABET[(bits % ALPHABET.len() as u64) as usize] as char);
        bits /= ALPHABET.len() as u64;
    }
    out
}

#[cfg(test)]
mod tests {
    //! Test data (tests/sdp): what Chrome 158 and str0m's SDP API (the core before it wrote its own
    //! SDP, and the media server's way: ICE-lite) produced, captured with tools/peer, plus one
    //! hand-written offer with a browser's other numbers (synthetic-firefox-like-offer.sdp; not
    //! captured from Firefox).
    use super::*;

    const CHROME_OFFER: &str = include_str!("../tests/sdp/chrome-offer.sdp");
    const CHROME_OFFER_ALL: &str = include_str!("../tests/sdp/chrome-offer-all-codecs.sdp");
    const CHROME_OFFER_SENDONLY: &str = include_str!("../tests/sdp/chrome-offer-sendonly-video.sdp");
    const CHROME_ANSWER: &str = include_str!("../tests/sdp/chrome-answer.sdp");
    const STR0M_OFFER: &str = include_str!("../tests/sdp/str0m-offer.sdp");
    const STR0M_OFFER_2: &str = include_str!("../tests/sdp/str0m-offer-2.sdp");
    const STR0M_ANSWER: &str = include_str!("../tests/sdp/str0m-answer.sdp");
    const STR0M_ANSWER_TO_CHROME: &str = include_str!("../tests/sdp/str0m-answer-to-chrome.sdp");
    const SERVER_OFFER: &str = include_str!("../tests/sdp/server-offer.sdp");
    const SERVER_ANSWER: &str = include_str!("../tests/sdp/server-answer.sdp");
    const FIREFOX_LIKE: &str = include_str!("../tests/sdp/synthetic-firefox-like-offer.sdp");

    const ALL: [&str; 11] = [
        CHROME_OFFER,
        CHROME_OFFER_ALL,
        CHROME_OFFER_SENDONLY,
        CHROME_ANSWER,
        STR0M_OFFER,
        STR0M_OFFER_2,
        STR0M_ANSWER,
        STR0M_ANSWER_TO_CHROME,
        SERVER_OFFER,
        SERVER_ANSWER,
        FIREFOX_LIKE,
    ];

    const FINGERPRINT: &str =
        "sha-256 E5:8C:2B:6E:74:A1:FE:00:BB:4D:50:EB:05:D1:BF:1C:60:21:77:BC:A0:D9:92:6D:5A:4E:11:66:64:2D:7B:AB";

    fn crlf(s: &str) -> String {
        s.replace("\r\n", "\n").replace('\n', "\r\n")
    }

    fn ours() -> Session {
        let mut s = Session::offer(42, "stream".into(), "cname".into(), ["ta".into(), "tv".into()], 262_144);
        s.section_mut(Kind::Audio).unwrap().ssrc = Some((1111, None));
        s.section_mut(Kind::Video).unwrap().ssrc = Some((2222, Some(3333)));
        s
    }

    fn transport(setup: Setup, candidates: &[String]) -> Transport<'_> {
        Transport { ufrag: "Xqufn23Y5PDl0XqR", pwd: "h4uAE7KvPhrhudPsG8skfX", fingerprint: FINGERPRINT, setup, candidates }
    }

    /// The session a captured offer's answer is read against: its mids and numbers, as the core
    /// keeps them when it answers that offer.
    fn session_of(offer: &str) -> Session {
        let plan = answer_plan(&Remote::parse(offer).unwrap()).unwrap();
        Session {
            id: 1,
            version: 1,
            numbers: plan.numbers,
            sections: plan.sections,
            stream: "s".into(),
            cname: "c".into(),
            max_message_size: 262_144,
        }
    }

    /// An offer with lines changed by `edit` (None drops the line).
    fn edited(text: &str, edit: impl Fn(&str) -> Option<String>) -> String {
        text.lines().filter_map(|l| edit(l.trim_end())).map(|l| l + "\r\n").collect()
    }

    #[test]
    fn our_offer_reads_back_as_written() {
        let candidates = vec!["candidate:1 1 udp 2130706175 127.0.0.1 5000 typ host ufrag Xqufn23Y5PDl0XqR".to_owned()];
        let mut session = ours();
        let text = session.write(&transport(Setup::ActPass, &candidates));
        assert!(text.ends_with("\r\n") && !text.contains("\r\r"));
        for line in ["a=group:BUNDLE 0 1 2", "m=audio 9 UDP/TLS/RTP/SAVPF 111", "m=video 9 UDP/TLS/RTP/SAVPF 108 109"] {
            assert!(text.contains(line), "{line}\n{text}");
        }
        assert_eq!(text.matches("a=candidate:").count(), 1, "candidates in the first section only");
        assert_eq!(text.matches("a=sendrecv").count(), 2);

        let remote = Remote::parse(&text).unwrap();
        let plan = answer_plan(&remote).unwrap();
        assert_eq!(plan.numbers, Numbers::profile());
        assert_eq!(plan.transport.ufrag, "Xqufn23Y5PDl0XqR");
        assert_eq!(plan.transport.pwd, "h4uAE7KvPhrhudPsG8skfX");
        assert_eq!(plan.transport.fingerprint.0, "sha-256");
        assert_eq!(plan.transport.fingerprint.1.len(), 32);
        assert_eq!(plan.transport.setup, Some(Setup::ActPass));
        assert_eq!(plan.candidates, candidates);
        let mids: Vec<&str> = plan.sections.iter().map(|s| s.mid.as_str()).collect();
        assert_eq!(mids, ["0", "1", "2"]);
        assert!(plan.sections.iter().all(|s| s.rejected.is_none() && s.direction == Direction::SendRecv));
        assert_eq!(plan.sections[0].exts, session.sections[0].exts);
        assert_eq!(plan.sections[1].exts, session.sections[1].exts);
        assert!(plan.sections[1].rtx);
        assert_eq!(plan.peers[0].unwrap().ssrc, Some((1111, None)));
        assert_eq!(plan.peers[1].unwrap().ssrc, Some((2222, Some(3333))));

        // The same session written as an answer reads as one.
        let answer = session.write(&transport(Setup::Passive, &[]));
        let read = read_answer(&ours(), &Remote::parse(&answer).unwrap()).unwrap();
        assert!(read.peers.iter().all(Option::is_some) && read.video_rtx && read.transport_cc);
        assert_eq!(read.transport.setup, Some(Setup::Passive));
    }

    #[test]
    fn line_endings_do_not_matter() {
        for text in ALL {
            let (lf, crlf) = (text.replace("\r\n", "\n"), crlf(text));
            let (a, b) = (Remote::parse(&lf).unwrap(), Remote::parse(&crlf).unwrap());
            assert_eq!(a.sections.len(), b.sections.len());
            assert_eq!(a.transport(), b.transport());
            assert_eq!(a.candidates(), b.candidates());
        }
    }

    #[test]
    fn chrome_offer_is_answered_with_its_numbers() {
        // Chrome's video first, its own extension ids (TWCC 4, mid 9, orientation 3, audio level
        // 14), its candidates in every section (bundle policy "balanced").
        let plan = answer_plan(&Remote::parse(CHROME_OFFER).unwrap()).unwrap();
        assert_eq!(plan.numbers.opus, 111);
        assert_eq!((plan.numbers.h264, plan.numbers.h264_rtx), (108, Some(109)));
        assert_eq!(plan.numbers.transport_cc, [true, true]);
        let mut exts = plan.numbers.exts.clone();
        exts.sort_by_key(|(id, _)| *id);
        assert_eq!(
            exts,
            [(2, Ext::AbsSendTime), (3, Ext::VideoOrientation), (4, Ext::TransportCc), (9, Ext::Mid), (14, Ext::AudioLevel)]
        );
        let kinds: Vec<(Kind, &str)> = plan.sections.iter().map(|s| (s.kind, s.mid.as_str())).collect();
        assert_eq!(kinds, [(Kind::Video, "0"), (Kind::Audio, "1"), (Kind::Application, "2")]);
        assert_eq!(
            plan.sections[0].exts,
            [(2, Ext::AbsSendTime), (3, Ext::VideoOrientation), (4, Ext::TransportCc), (9, Ext::Mid)]
        );
        assert_eq!(plan.sections[1].exts, [(14, Ext::AudioLevel), (2, Ext::AbsSendTime), (4, Ext::TransportCc), (9, Ext::Mid)]);
        assert_eq!(plan.peers[0].unwrap().ssrc, Some((2299741147, Some(4039026070))));
        assert_eq!(plan.peers[1].unwrap().ssrc, Some((991781596, None)));
        assert_eq!(plan.transport.ufrag, "t0wv");
        assert_eq!(plan.transport.setup, Some(Setup::ActPass));
        // 7 UDP of their own per section; the 7 TCP ones (port 9) are the same in each: once.
        assert_eq!(CHROME_OFFER.matches("a=candidate:").count(), 42);
        assert_eq!(plan.candidates.len(), 28);

        let mut session = session_of(CHROME_OFFER);
        session.sections[0].ssrc = Some((5, Some(6)));
        session.sections[1].ssrc = Some((7, None));
        let text = session.write(&transport(Setup::Passive, &[]));
        assert!(text.contains("a=group:BUNDLE 0 1 2\r\n"));
        assert!(text.contains("m=video 9 UDP/TLS/RTP/SAVPF 108 109\r\n"));
        assert!(text.contains("a=extmap:9 urn:ietf:params:rtp-hdrext:sdes:mid"));
        assert!(text.contains("a=extmap:14 urn:ietf:params:rtp-hdrext:ssrc-audio-level"));
        assert!(!text.contains("toffset") && !text.contains("rtp-stream-id"), "what the profile does not use is left out");
    }

    #[test]
    fn chrome_offer_with_every_codec_takes_the_profiles_h264() {
        let plan = answer_plan(&Remote::parse(CHROME_OFFER_ALL).unwrap()).unwrap();
        // 102 (42001f, plain Baseline) comes first in Chrome's list: not the profile.
        assert_eq!((plan.numbers.h264, plan.numbers.h264_rtx), (108, Some(109)));
        assert_eq!(plan.numbers.opus, 111);
        assert!(plan.sections.iter().all(|s| s.rejected.is_none()));
        let mut session = session_of(CHROME_OFFER_ALL);
        let text = session.write(&transport(Setup::Passive, &[]));
        assert_eq!(text.matches("a=rtpmap:").count(), 3, "Opus, H.264, its RTX:\n{text}");
    }

    #[test]
    fn a_publishers_sendonly_offer_is_answered_recvonly() {
        let plan = answer_plan(&Remote::parse(CHROME_OFFER_SENDONLY).unwrap()).unwrap();
        assert_eq!(plan.sections.len(), 1);
        assert_eq!(plan.sections[0].direction, Direction::RecvOnly);
        let peer = plan.peers[0].unwrap();
        assert!(peer.sends && !peer.receives && peer.ssrc.is_some());
        let mut session = session_of(CHROME_OFFER_SENDONLY);
        session.sections[0].ssrc = Some((5, Some(6)));
        let text = session.write(&transport(Setup::Passive, &[]));
        assert!(text.contains("a=recvonly") && !text.contains("a=ssrc:"), "nothing sent, nothing announced:\n{text}");
    }

    #[test]
    fn other_numbers_of_a_browser_are_taken() {
        // Opus 109, H.264 126 with RTX 127 (97 is packetization-mode 0), TWCC on video only at 7,
        // mid at 3, the fingerprint at session level.
        let plan = answer_plan(&Remote::parse(FIREFOX_LIKE).unwrap()).unwrap();
        assert_eq!(plan.numbers.opus, 109);
        assert_eq!((plan.numbers.h264, plan.numbers.h264_rtx), (126, Some(127)));
        assert_eq!(plan.numbers.transport_cc, [false, true]);
        assert_eq!(plan.sections[0].exts, [(1, Ext::AudioLevel), (3, Ext::Mid)]);
        assert_eq!(plan.sections[1].exts, [(3, Ext::Mid), (4, Ext::AbsSendTime), (7, Ext::TransportCc)]);
        assert_eq!(plan.transport.fingerprint.1.len(), 32);
        assert_eq!(plan.peers[1].unwrap().ssrc, Some((2914623847, Some(1061547202))));
        let mut session = session_of(FIREFOX_LIKE);
        let text = session.write(&transport(Setup::Passive, &[]));
        for line in [
            "m=audio 9 UDP/TLS/RTP/SAVPF 109\r\n",
            "a=rtpmap:109 opus/48000/2",
            "m=video 9 UDP/TLS/RTP/SAVPF 126 127\r\n",
            "a=fmtp:127 apt=126",
            "a=rtcp-fb:126 transport-cc",
            "a=extmap:7 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
        ] {
            assert!(text.contains(line), "{line}\n{text}");
        }
        assert!(!text.contains("a=rtcp-fb:109 transport-cc"), "not offered for audio");
    }

    #[test]
    fn str0m_offers_are_answered() {
        for offer in [STR0M_OFFER, STR0M_OFFER_2, SERVER_OFFER] {
            let plan = answer_plan(&Remote::parse(offer).unwrap()).unwrap();
            assert_eq!(plan.numbers, Numbers::profile(), "rid / repaired-rid are not the profile's");
            assert!(plan.sections.iter().all(|s| s.rejected.is_none()));
            assert!(plan.peers.iter().take(2).all(|p| p.unwrap().ssrc.is_some()));
            assert_eq!(plan.candidates.len(), offer.matches("a=candidate").count());
        }
    }

    #[test]
    fn answers_of_chrome_and_str0m_are_read() {
        // Chrome answering str0m's offer: setup active, a=ssrc with cname only.
        let read = read_answer(&session_of(STR0M_OFFER), &Remote::parse(CHROME_ANSWER).unwrap()).unwrap();
        assert!(read.peers.iter().all(Option::is_some) && read.video_rtx && read.transport_cc);
        assert_eq!(read.transport.setup, Some(Setup::Active));
        assert_eq!(read.transport.ufrag, "5zbd");
        assert_eq!(read.peers[0].unwrap().ssrc, Some((3278110815, None)));
        assert_eq!(read.peers[1].unwrap().ssrc, Some((1380031890, Some(3829242582))));
        assert_eq!(read.candidates.len(), 14);

        // str0m answering: passive; the media server's is ICE-lite.
        for (offer, answer, lite) in [(STR0M_OFFER_2, STR0M_ANSWER, false), (SERVER_OFFER, SERVER_ANSWER, true)] {
            let read = read_answer(&session_of(offer), &Remote::parse(answer).unwrap()).unwrap();
            assert!(read.peers.iter().all(Option::is_some) && read.video_rtx && read.transport_cc);
            assert_eq!(read.transport.setup, Some(Setup::Passive));
            assert_eq!(read.transport.ice_lite, lite);
            assert_eq!(read.candidates.len(), 1);
        }
        // str0m answering Chrome's offer (the raw peer of tools/peer).
        let read = read_answer(&session_of(CHROME_OFFER), &Remote::parse(STR0M_ANSWER_TO_CHROME).unwrap()).unwrap();
        assert!(read.peers.iter().all(Option::is_some) && read.video_rtx);
    }

    #[test]
    fn what_cannot_be_used_is_answered_rejected() {
        // Video with VP8 only: rejected, the rest taken.
        let vp8 = edited(CHROME_OFFER, |l| {
            Some(match l {
                "m=video 60014 UDP/TLS/RTP/SAVPF 108 109" => "m=video 60014 UDP/TLS/RTP/SAVPF 96".into(),
                l if l.starts_with("a=rtpmap:108") => "a=rtpmap:96 VP8/90000".into(),
                l => l.to_owned(),
            })
        });
        let plan = answer_plan(&Remote::parse(&vp8).unwrap()).unwrap();
        assert!(plan.sections[0].rejected.is_some() && plan.peers[0].is_none());
        assert!(plan.sections[1].rejected.is_none() && plan.sections[2].rejected.is_none());
        let mut session = session_of(&vp8);
        let text = session.write(&transport(Setup::Passive, &[]));
        assert!(text.contains("a=group:BUNDLE 1 2\r\n"), "{text}");
        assert!(text.contains("m=video 0 UDP/TLS/RTP/SAVPF 96\r\nc=IN IP4 0.0.0.0\r\na=mid:0\r\n"), "{text}");
        assert!(read_answer(&session, &Remote::parse(&text).unwrap()).is_ok(), "an answer of the other two");

        // Nothing usable at all: the VP8 video alone.
        let only_video = vp8.split("m=audio").next().unwrap().replace("a=group:BUNDLE 0 1 2", "a=group:BUNDLE 0");
        assert!(answer_plan(&Remote::parse(&only_video).unwrap()).is_err());

        // A second audio section, a section outside BUNDLE, a mid str0m cannot keep, Opus at a
        // payload type RTCP overlaps.
        let mut twice = CHROME_OFFER.replace("\r\n", "\n");
        let audio = twice[twice.find("m=audio").unwrap()..twice.find("m=application").unwrap()].to_owned();
        twice.push_str(&audio.replace("a=mid:1", "a=mid:3"));
        let twice = twice.replace("a=group:BUNDLE 0 1 2", "a=group:BUNDLE 0 1 2 3");
        let plan = answer_plan(&Remote::parse(&twice).unwrap()).unwrap();
        assert_eq!(plan.sections.len(), 4);
        assert!(plan.sections[3].rejected.is_some() && plan.sections[1].rejected.is_none());
        let outside = CHROME_OFFER.replace("a=group:BUNDLE 0 1 2", "a=group:BUNDLE 1 2");
        assert!(answer_plan(&Remote::parse(&outside).unwrap()).unwrap().sections[0].rejected.is_some());
        let long = CHROME_OFFER.replace("a=mid:1", "a=mid:waytoolongforstr0m1").replace("BUNDLE 0 1 2", "BUNDLE 0 waytoolongforstr0m1 2");
        assert!(answer_plan(&Remote::parse(&long).unwrap()).unwrap().sections[1].rejected.is_some());
        let rtcp = CHROME_OFFER.replace("111 63 9", "72 63 9").replace("a=rtpmap:111 ", "a=rtpmap:72 ");
        assert!(answer_plan(&Remote::parse(&rtcp).unwrap()).unwrap().sections[1].rejected.is_some());
    }

    #[test]
    fn what_no_answer_can_fix_is_an_error() {
        assert!(Remote::parse("not a description").is_err());
        assert!(Remote::parse("").is_err());
        let no_fingerprint = edited(CHROME_OFFER, |l| (!l.starts_with("a=fingerprint")).then(|| l.to_owned()));
        assert!(answer_plan(&Remote::parse(&no_fingerprint).unwrap()).is_err());
        let sha512 = CHROME_OFFER.replace("a=fingerprint:sha-256", "a=fingerprint:sha-512");
        assert!(answer_plan(&Remote::parse(&sha512).unwrap()).is_err());
        let no_ice = edited(CHROME_OFFER, |l| (!l.starts_with("a=ice-pwd")).then(|| l.to_owned()));
        assert!(answer_plan(&Remote::parse(&no_ice).unwrap()).is_err());
        let bad_ice = CHROME_OFFER.replace("a=ice-ufrag:t0wv", "a=ice-ufrag:t0;wv");
        assert!(answer_plan(&Remote::parse(&bad_ice).unwrap()).is_err());
        let no_bundle = edited(CHROME_OFFER, |l| (!l.starts_with("a=group")).then(|| l.to_owned()));
        assert!(answer_plan(&Remote::parse(&no_bundle).unwrap()).is_err());
    }

    #[test]
    fn values_str0m_could_trip_over_are_left_out() {
        // The same SSRC in two sections, an extension id past the one-byte range, one id for two
        // extensions.
        let dup = CHROME_OFFER.replace("991781596", "2299741147");
        let plan = answer_plan(&Remote::parse(&dup).unwrap()).unwrap();
        assert!(plan.peers[0].unwrap().ssrc.is_some() && plan.peers[1].unwrap().ssrc.is_none());
        let far = CHROME_OFFER.replace(
            "a=extmap:9 urn:ietf:params:rtp-hdrext:sdes:mid",
            "a=extmap:15 urn:ietf:params:rtp-hdrext:sdes:mid",
        );
        assert!(answer_plan(&Remote::parse(&far).unwrap()).unwrap().numbers.ext_id(Ext::Mid).is_none());
        let clash = CHROME_OFFER.replace(
            "a=extmap:14 urn:ietf:params:rtp-hdrext:ssrc-audio-level",
            "a=extmap:3 urn:ietf:params:rtp-hdrext:ssrc-audio-level",
        );
        let plan = answer_plan(&Remote::parse(&clash).unwrap()).unwrap();
        assert_eq!(plan.numbers.ext_id(Ext::VideoOrientation), Some(3));
        assert_eq!(plan.numbers.ext_id(Ext::AudioLevel), None, "id 3 is the video's orientation already");
    }

    #[test]
    fn an_answer_must_keep_our_numbers() {
        let mut session = ours();
        let answer = session.write(&transport(Setup::Passive, &[]));
        let renumbered =
            answer.replace("UDP/TLS/RTP/SAVPF 111", "UDP/TLS/RTP/SAVPF 112").replace("a=rtpmap:111", "a=rtpmap:112");
        assert!(read_answer(&ours(), &Remote::parse(&renumbered).unwrap()).is_err());
        let ext = answer.replace(
            "a=extmap:4 urn:ietf:params:rtp-hdrext:sdes:mid",
            "a=extmap:5 urn:ietf:params:rtp-hdrext:sdes:mid",
        );
        assert!(read_answer(&ours(), &Remote::parse(&ext).unwrap()).is_err());
        let fewer = answer.split("m=application").next().unwrap().replace("BUNDLE 0 1 2", "BUNDLE 0 1");
        assert!(read_answer(&ours(), &Remote::parse(&fewer).unwrap()).is_err(), "the sections are the offer's");
        let other_mid = answer.replace("a=mid:1", "a=mid:7");
        assert!(read_answer(&ours(), &Remote::parse(&other_mid).unwrap()).is_err());
        // Rejecting video and leaving out the RTX are allowed.
        let no_video = answer
            .replace("m=video 9 UDP/TLS/RTP/SAVPF 108 109", "m=video 0 UDP/TLS/RTP/SAVPF 108 109")
            .replace("BUNDLE 0 1 2", "BUNDLE 0 2");
        let read = read_answer(&ours(), &Remote::parse(&no_video).unwrap()).unwrap();
        assert!(read.peers[0].is_some() && read.peers[1].is_none() && read.peers[2].is_some());
        let no_rtx = answer.replace("108 109", "108");
        assert!(!read_answer(&ours(), &Remote::parse(&no_rtx).unwrap()).unwrap().video_rtx);
    }

    #[test]
    fn constrained_baseline_is_recognised() {
        for (fmtp, ok) in [
            ("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f", true),
            ("profile-level-id=42E01F;packetization-mode=1", true),
            ("packetization-mode=1; profile-level-id=42e034", true),
            ("profile-level-id=4d801f;packetization-mode=1", true),
            ("profile-level-id=42001f;packetization-mode=1", false),
            ("profile-level-id=42e01f;packetization-mode=0", false),
            ("profile-level-id=42e01f", false),
            ("profile-level-id=64001f;packetization-mode=1", false),
            ("profile-level-id=42e0;packetization-mode=1", false),
            ("profile-level-id=zze01f;packetization-mode=1", false),
        ] {
            assert_eq!(is_profile_h264(fmtp), ok, "{fmtp}");
        }
    }

    /// xorshift64*: deterministic noise for the mutations.
    struct Noise(u64);
    impl Noise {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }
    }

    /// Cuts `line` at a char boundary.
    fn cut_at(line: &str, noise: &mut Noise) -> usize {
        let at = noise.below(line.len() + 1);
        (at..=line.len()).find(|i| line.is_char_boundary(*i)).unwrap_or(line.len())
    }

    /// One mutation of a description: lines dropped, doubled, moved or cut, a number replaced by
    /// an extreme one, junk inserted, the rest cut off.
    pub(crate) fn mutate(text: &str, seed: u64) -> String {
        let noise = &mut Noise(seed | 1);
        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
        for _ in 0..1 + noise.below(4) {
            if lines.is_empty() {
                break;
            }
            let at = noise.below(lines.len());
            match noise.below(8) {
                0 => {
                    lines.remove(at);
                }
                1 => {
                    let line = lines[at].clone();
                    let to = noise.below(lines.len());
                    lines.insert(to, line);
                }
                2 => {
                    let to = noise.below(lines.len());
                    lines.swap(at, to);
                }
                3 => {
                    let cut = cut_at(&lines[at], noise);
                    lines[at].truncate(cut);
                }
                4 | 5 => {
                    const EXTREMES: [&str; 12] =
                        ["0", "1", "15", "16", "64", "95", "127", "128", "255", "65535", "4294967295", "18446744073709551616"];
                    let line = lines[at].clone();
                    let digits: Vec<usize> = line.char_indices().filter(|(_, c)| c.is_ascii_digit()).map(|(i, _)| i).collect();
                    if !digits.is_empty() {
                        let start = digits[noise.below(digits.len())];
                        let end = line[start..].find(|c: char| !c.is_ascii_digit()).map_or(line.len(), |e| start + e);
                        lines[at] = format!("{}{}{}", &line[..start], EXTREMES[noise.below(EXTREMES.len())], &line[end..]);
                    }
                }
                6 => {
                    const JUNK: [&str; 8] = ["a=", "=", ":", "m=video", "a=mid:", "a=ssrc-group:FID", "\u{fffd}é", " \t "];
                    let line = lines[at].clone();
                    let cut = cut_at(&line, noise);
                    lines[at] = format!("{}{}{}", &line[..cut], JUNK[noise.below(JUNK.len())], &line[cut..]);
                }
                _ => lines.truncate(at),
            }
        }
        lines.join(if noise.below(2) == 0 { "\r\n" } else { "\n" })
    }

    /// Everything a description goes through, on any input: it may fail, never panic; and what
    /// we write from an offer we take is a description we take back.
    fn everything(text: &str) {
        let Ok(remote) = Remote::parse(text) else { return };
        let _ = remote.transport();
        let _ = remote.candidates();
        if let Ok(plan) = answer_plan(&remote) {
            assert!(plan.numbers.exts.iter().all(|(id, _)| (1..=14).contains(id)));
            assert!(dynamic_pt(plan.numbers.opus) && dynamic_pt(plan.numbers.h264));
            assert!(plan.numbers.h264_rtx.is_none_or(|rtx| dynamic_pt(rtx) && rtx != plan.numbers.h264));
            assert!(plan.sections.iter().filter(|s| s.rejected.is_none()).all(|s| usable_mid(&s.mid)));
            let mut session = Session {
                id: 1,
                version: 1,
                numbers: plan.numbers.clone(),
                sections: plan.sections.clone(),
                stream: "s".into(),
                cname: "c".into(),
                max_message_size: 1,
            };
            let text = session.write(&transport(Setup::Passive, &plan.candidates));
            let again = Remote::parse(&text).unwrap();
            assert!(answer_plan(&again).is_ok(), "{text}");
            assert!(read_answer(&session, &again).is_ok(), "{text}");
        }
        let _ = read_answer(&ours(), &remote);
    }

    #[test]
    fn no_input_panics() {
        for text in ALL {
            for cut in (0..text.len()).filter(|i| text.is_char_boundary(*i)) {
                everything(&text[..cut]);
            }
        }
        let mut noise = Noise(0x9e37_79b9_7f4a_7c15);
        for round in 0..30_000 {
            let base = ALL[round % ALL.len()];
            let mut text = mutate(base, noise.next());
            if round % 3 == 0 {
                text = mutate(&text, noise.next());
            }
            everything(&text);
        }
        for _ in 0..2_000 {
            let len = noise.below(400);
            let bytes: Vec<u8> = (0..len).map(|_| b"am=:/ 0123456789abcdefv\r\n"[noise.below(25)]).collect();
            everything(&String::from_utf8_lossy(&bytes));
        }
    }

    #[test]
    fn the_writer_says_what_str0m_is_configured_with() {
        // profile.rs builds str0m from the same numbers: its parameters are what we write.
        let numbers = Numbers::profile();
        let rtc = crate::profile::config_with(&numbers).build(std::time::Instant::now());
        let params = rtc.codec_config().params();
        assert_eq!(params.len(), 2);
        let (opus, h264) = (&params[0], &params[1]);
        assert_eq!((*opus.pt(), opus.resend()), (numbers.opus, None));
        assert_eq!((*h264.pt(), h264.resend().map(|p| *p)), (numbers.h264, numbers.h264_rtx));
        assert!(opus.fb_transport_cc() && !opus.fb_nack() && !opus.fb_pli() && !opus.fb_fir() && !opus.fb_remb());
        // "transport-cc", "goog-remb", "ccm fir", "nack", "nack pli" in Session::write.
        assert!(h264.fb_transport_cc() && h264.fb_remb() && h264.fb_fir() && h264.fb_nack() && h264.fb_pli());
        assert_eq!(h264.spec().format.profile_level_id, Some(0x42e01f));
        assert_eq!(h264.spec().format.packetization_mode, Some(1));
        assert_eq!(h264.spec().format.level_asymmetry_allowed, Some(true));
        assert_eq!(opus.spec().format.min_p_time, Some(10));
        assert_eq!(opus.spec().format.use_inband_fec, Some(true));
        assert_eq!(
            str0m::RtcConfig::default().sctp_max_message_size(),
            crate::connection::MAX_MESSAGE_SIZE,
            "a=max-message-size is what our SCTP takes"
        );
    }
}
