//! The STUN client: binding requests, for the server-reflexive candidates.
//!
//! str0m runs ICE's connectivity checks but gathers no candidates; finding the server-reflexive
//! one (our address as a STUN server sees it) is ours. Sans-IO: no socket, no clock. A
//! connection's thread offers EVERY datagram of its socket to [`Binding::handle_input`] first and
//! hands what is not consumed to str0m, whose own checks are STUN too — so a datagram is consumed
//! only when it is a response from the server's address carrying the transaction id of our request.
//!
//! Three layers, so TURN (RFC 8656) can sit beside [`Binding`] without a rewrite:
//!   [`parse_ice_url`]               the `stun:` / `turn:` / `turns:` URLs (RFC 7064 / 7065)
//!   [`Message`], [`MessageWriter`]  the wire format (RFC 8489): a header, attributes, FINGERPRINT
//!   [`Binding`]                     one transaction and its retransmissions (RFC 8489 §6.2.1)

use std::fmt;
use std::hash::{BuildHasher, RandomState};
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// ---- ICE server URLs (RFC 7064 / 7065) ----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Stun,
    Turn,
    Turns,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Udp,
    Tcp,
}

/// One `tinyrtc_ice_server.url`. The host is not resolved: a name or an IP literal (IPv6 without its
/// brackets); `(host.as_str(), port).to_socket_addrs()` takes either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IceUrl {
    pub scheme: Scheme,
    pub host: String,
    /// The URL's, else 3478 (`stun:`, `turn:`) or 5349 (`turns:`).
    pub port: u16,
    /// How the server is reached: `stun:` UDP; `turn:` UDP unless `?transport=tcp`; `turns:` TCP
    /// (TLS).
    pub transport: Transport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlError {
    /// Not `stun:`, `turn:` or `turns:`.
    Scheme,
    /// Empty, a character no host name has, an IPv6 literal without brackets, brackets around
    /// something else.
    Host,
    /// Not 1..=65535.
    Port,
    /// A query on `stun:`; on `turn:` anything but `transport=udp|tcp`, on `turns:` anything but
    /// `transport=tcp`.
    Query,
}

impl fmt::Display for UrlError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            UrlError::Scheme => "not a stun: / turn: / turns: URL",
            UrlError::Host => "bad host",
            UrlError::Port => "bad port",
            UrlError::Query => "bad ?transport=",
        })
    }
}

impl std::error::Error for UrlError {}

/// "stun:host[:port]", "turn:host[:port][?transport=udp|tcp]",
/// "turns:host[:port][?transport=tcp]". Schemes and the query are case-insensitive; no "//", no
/// user info, no zone id in an IPv6 literal.
pub fn parse_ice_url(url: &str) -> Result<IceUrl, UrlError> {
    let (scheme, rest) = url.split_once(':').ok_or(UrlError::Scheme)?;
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "stun" => Scheme::Stun,
        "turn" => Scheme::Turn,
        "turns" => Scheme::Turns,
        _ => return Err(UrlError::Scheme),
    };
    let (authority, query) = match rest.split_once('?') {
        Some((authority, query)) => (authority, Some(query)),
        None => (rest, None),
    };
    let transport = match (scheme, query) {
        (Scheme::Turns, None) => Transport::Tcp,
        (_, None) => Transport::Udp,
        (Scheme::Stun, Some(_)) => return Err(UrlError::Query),
        (_, Some(query)) => {
            let (key, value) = query.split_once('=').ok_or(UrlError::Query)?;
            if !key.eq_ignore_ascii_case("transport") {
                return Err(UrlError::Query);
            }
            if value.eq_ignore_ascii_case("tcp") {
                Transport::Tcp
            } else if value.eq_ignore_ascii_case("udp") && scheme == Scheme::Turn {
                Transport::Udp
            } else {
                return Err(UrlError::Query);
            }
        }
    };
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (literal, after) = bracketed.split_once(']').ok_or(UrlError::Host)?;
        literal.parse::<Ipv6Addr>().map_err(|_| UrlError::Host)?;
        let port = match after {
            "" => None,
            _ => Some(after.strip_prefix(':').ok_or(UrlError::Host)?),
        };
        (literal, port)
    } else {
        let (host, port) = match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        };
        let name = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_');
        // A second ':' is an IPv6 literal without its brackets.
        if host.is_empty() || !host.bytes().all(name) || port.is_some_and(|p| p.contains(':')) {
            return Err(UrlError::Host);
        }
        (host, port)
    };
    let port = match port {
        None if scheme == Scheme::Turns => 5349,
        None => 3478,
        // Digits only: `parse` would take a "+".
        Some(p) => match p.parse::<u16>() {
            Ok(port) if port != 0 && p.bytes().all(|b| b.is_ascii_digit()) => port,
            _ => return Err(UrlError::Port),
        },
    };
    Ok(IceUrl {
        scheme,
        host: host.to_owned(),
        port,
        transport,
    })
}

// ---- the wire format (RFC 8489 §5, §14) -------------------------------------------------------

pub const HEADER_LEN: usize = 20;
pub const MAGIC_COOKIE: u32 = 0x2112_A442;
const FINGERPRINT_XOR: u32 = 0x5354_554E;

/// The methods (12 bits). TURN adds Allocate 0x003, Refresh 0x004, Send 0x006, Data 0x007,
/// CreatePermission 0x008, ChannelBind 0x009.
pub const BINDING: u16 = 0x001;

/// Attribute types. 0x0000..=0x7FFF are comprehension-required: a response carrying one we do not
/// know fails its transaction (RFC 8489 §6.3.3); 0x8000..=0xFFFF are skipped when unknown.
pub mod attr {
    pub const MAPPED_ADDRESS: u16 = 0x0001;
    pub const USERNAME: u16 = 0x0006;
    pub const MESSAGE_INTEGRITY: u16 = 0x0008;
    pub const ERROR_CODE: u16 = 0x0009;
    pub const UNKNOWN_ATTRIBUTES: u16 = 0x000A;
    pub const REALM: u16 = 0x0014;
    pub const NONCE: u16 = 0x0015;
    pub const MESSAGE_INTEGRITY_SHA256: u16 = 0x001C;
    pub const PASSWORD_ALGORITHM: u16 = 0x001D;
    pub const USERHASH: u16 = 0x001E;
    pub const XOR_MAPPED_ADDRESS: u16 = 0x0020;
    pub const SOFTWARE: u16 = 0x8022;
    pub const FINGERPRINT: u16 = 0x8028;
}

/// The two class bits, C1 C0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Request = 0b00,
    Indication = 0b01,
    Success = 0b10,
    Error = 0b11,
}

/// 96 bits per transaction; retransmissions keep it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransactionId(pub [u8; 12]);

impl TransactionId {
    /// Without a random-number crate: SipHash keyed by std's `RandomState` (keys drawn from the
    /// OS's random source once per thread, advanced on every call) over a process-wide counter and
    /// `now` (only mixed in; the clock is not read). Unique within the process and unguessable
    /// off the path as long as those keys are; not a vetted CSPRNG.
    pub fn random(now: Instant) -> TransactionId {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let keys = RandomState::new();
        let a = keys.hash_one((n, now, 0u8)).to_le_bytes();
        let b = keys.hash_one((n, now, 1u8)).to_le_bytes();
        let mut id = [0; 12];
        id[..8].copy_from_slice(&a);
        id[8..].copy_from_slice(&b[..4]);
        TransactionId(id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// Shorter than a header, a first byte with either of its top two bits set, no magic cookie.
    NotStun,
    /// The header's length is not a multiple of 4, or not the rest of the datagram.
    Length,
    /// An attribute runs past the end.
    Attribute,
    /// A FINGERPRINT that is not the last attribute, not 4 bytes, or does not match.
    Fingerprint,
}

/// First two bits zero and the magic cookie: STUN rather than RTP / RTCP, DTLS or a TURN
/// ChannelData (RFC 7983). For logging; [`Message::parse`] is what decides.
pub fn is_stun(datagram: &[u8]) -> bool {
    datagram.len() >= HEADER_LEN
        && datagram[0] & 0xC0 == 0
        && datagram[4..8] == MAGIC_COOKIE.to_be_bytes()
}

/// A parsed message, borrowing the datagram. Every attribute's framing and the FINGERPRINT (when
/// there is one) are checked by [`Message::parse`]; the values are read on demand.
#[derive(Debug, Clone, Copy)]
pub struct Message<'a> {
    bytes: &'a [u8],
}

impl<'a> Message<'a> {
    /// One whole message: a UDP datagram is exactly one.
    pub fn parse(bytes: &'a [u8]) -> Result<Message<'a>, ParseError> {
        if !is_stun(bytes) {
            return Err(ParseError::NotStun);
        }
        let len = usize::from(be16(bytes, 2).ok_or(ParseError::NotStun)?);
        if len % 4 != 0 || HEADER_LEN + len != bytes.len() {
            return Err(ParseError::Length);
        }
        let mut at = HEADER_LEN;
        while at < bytes.len() {
            let ty = be16(bytes, at).ok_or(ParseError::Attribute)?;
            let value_len = usize::from(be16(bytes, at + 2).ok_or(ParseError::Attribute)?);
            let next = at + 4 + pad4(value_len);
            if next > bytes.len() {
                return Err(ParseError::Attribute);
            }
            if ty == attr::FINGERPRINT {
                let value = be32(bytes, at + 4);
                if value_len != 4
                    || next != bytes.len()
                    || value != Some(fingerprint_of(&bytes[..at]))
                {
                    return Err(ParseError::Fingerprint);
                }
            }
            at = next;
        }
        Ok(Message { bytes })
    }

    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    pub fn class(&self) -> Class {
        let t = self.message_type();
        match ((t >> 4) & 1) | ((t >> 7) & 2) {
            0 => Class::Request,
            1 => Class::Indication,
            2 => Class::Success,
            _ => Class::Error,
        }
    }

    pub fn method(&self) -> u16 {
        let t = self.message_type();
        (t & 0x000F) | ((t >> 1) & 0x0070) | ((t >> 2) & 0x0F80)
    }

    pub fn transaction_id(&self) -> TransactionId {
        let mut id = [0; 12];
        id.copy_from_slice(&self.bytes[8..HEADER_LEN]);
        TransactionId(id)
    }

    /// Every attribute in order, as (type, value without padding).
    pub fn attributes(&self) -> Attributes<'a> {
        Attributes {
            rest: &self.bytes[HEADER_LEN..],
        }
    }

    /// The first attribute of a type. What follows MESSAGE-INTEGRITY is ignored, but for
    /// MESSAGE-INTEGRITY-SHA256 and FINGERPRINT (RFC 8489 §14.5, §14.6).
    pub fn get(&self, ty: u16) -> Option<&'a [u8]> {
        self.processed()
            .find(|&(t, _)| t == ty)
            .map(|(_, value)| value)
    }

    /// A MAPPED-ADDRESS-shaped attribute (MAPPED-ADDRESS; ALTERNATE-SERVER).
    pub fn address(&self, ty: u16) -> Option<SocketAddr> {
        decode_address(self.get(ty)?, &[0; 16])
    }

    /// An XOR-MAPPED-ADDRESS-shaped attribute (XOR-MAPPED-ADDRESS; TURN's XOR-RELAYED-ADDRESS and
    /// XOR-PEER-ADDRESS): XORed with the magic cookie and the transaction id.
    pub fn xor_address(&self, ty: u16) -> Option<SocketAddr> {
        decode_address(self.get(ty)?, &self.bytes[4..HEADER_LEN])
    }

    /// ERROR-CODE as (300..=699, the reason phrase). A phrase that is not UTF-8 is cut where it
    /// stops being UTF-8. None when absent or malformed.
    pub fn error_code(&self) -> Option<(u16, &'a str)> {
        let value = self.get(attr::ERROR_CODE)?;
        let class = u16::from(*value.get(2)? & 0x07);
        let number = u16::from(*value.get(3)?);
        if !(3..=6).contains(&class) || number > 99 {
            return None;
        }
        let phrase = &value[4..];
        let phrase = match std::str::from_utf8(phrase) {
            Ok(phrase) => phrase,
            Err(e) => std::str::from_utf8(&phrase[..e.valid_up_to()]).unwrap_or(""),
        };
        Some((class * 100 + number, phrase))
    }

    /// The first comprehension-required attribute (type < 0x8000) not in `known`.
    pub fn unknown_required(&self, known: &[u16]) -> Option<u16> {
        self.processed()
            .map(|(ty, _)| ty)
            .find(|&ty| ty < 0x8000 && !known.contains(&ty))
    }

    fn message_type(&self) -> u16 {
        u16::from_be_bytes([self.bytes[0], self.bytes[1]])
    }

    fn processed(&self) -> impl Iterator<Item = (u16, &'a [u8])> {
        let mut after_integrity = false;
        self.attributes().filter(move |&(ty, _)| {
            let integrity = ty == attr::MESSAGE_INTEGRITY || ty == attr::MESSAGE_INTEGRITY_SHA256;
            let keep =
                !after_integrity || ty == attr::MESSAGE_INTEGRITY_SHA256 || ty == attr::FINGERPRINT;
            after_integrity |= integrity;
            keep
        })
    }
}

pub struct Attributes<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for Attributes<'a> {
    type Item = (u16, &'a [u8]);

    fn next(&mut self) -> Option<(u16, &'a [u8])> {
        let ty = be16(self.rest, 0)?;
        let len = usize::from(be16(self.rest, 2)?);
        let value = self.rest.get(4..4 + len)?;
        self.rest = self.rest.get(4 + pad4(len)..).unwrap_or(&[]);
        Some((ty, value))
    }
}

/// Builds one message. The header's length is kept current as attributes are appended, so what is
/// computed over the message so far (FINGERPRINT here; MESSAGE-INTEGRITY for TURN) sees the right
/// one. The attributes of one message stay well under 64 KiB: it is meant to fit a datagram.
pub struct MessageWriter {
    buf: Vec<u8>,
}

impl MessageWriter {
    pub fn new(method: u16, class: Class, id: TransactionId) -> MessageWriter {
        let c = class as u16;
        let ty = (method & 0x000F)
            | ((method & 0x0070) << 1)
            | ((method & 0x0F80) << 2)
            | ((c & 1) << 4)
            | ((c & 2) << 7);
        let mut buf = Vec::with_capacity(HEADER_LEN + 8); // a Binding request: header + FINGERPRINT
        buf.extend_from_slice(&ty.to_be_bytes());
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        buf.extend_from_slice(&id.0);
        MessageWriter { buf }
    }

    /// Appends one attribute, zero-padded to 4 bytes.
    pub fn attribute(&mut self, ty: u16, value: &[u8]) -> &mut MessageWriter {
        debug_assert!(value.len() <= 0xFFFF);
        let end = self.buf.len() + 4 + pad4(value.len());
        let value_len = value.len() as u16;
        self.buf.extend_from_slice(&ty.to_be_bytes());
        self.buf.extend_from_slice(&value_len.to_be_bytes());
        self.buf.extend_from_slice(value);
        self.buf.resize(end, 0);
        let len = (end - HEADER_LEN) as u16;
        self.buf[2..4].copy_from_slice(&len.to_be_bytes());
        self
    }

    /// Appends an XOR-MAPPED-ADDRESS-shaped attribute.
    pub fn xor_address(&mut self, ty: u16, addr: SocketAddr) -> &mut MessageWriter {
        let mut mask = [0; 16];
        mask.copy_from_slice(&self.buf[4..HEADER_LEN]);
        let mut value = [0; 20];
        let port = addr.port() ^ u16::from_be_bytes([mask[0], mask[1]]);
        value[2..4].copy_from_slice(&port.to_be_bytes());
        let len = match addr.ip() {
            IpAddr::V4(ip) => {
                value[1] = 0x01;
                value[4..8].copy_from_slice(&ip.octets());
                8
            }
            IpAddr::V6(ip) => {
                value[1] = 0x02;
                value[4..20].copy_from_slice(&ip.octets());
                20
            }
        };
        for (byte, m) in value[4..len].iter_mut().zip(mask) {
            *byte ^= m;
        }
        self.attribute(ty, &value[..len])
    }

    /// Appends FINGERPRINT; nothing may follow it.
    pub fn fingerprint(&mut self) -> &mut MessageWriter {
        let at = self.buf.len();
        self.attribute(attr::FINGERPRINT, &[0; 4]);
        let crc = fingerprint_of(&self.buf[..at]);
        self.buf[at + 4..].copy_from_slice(&crc.to_be_bytes());
        self
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

/// `mask` = the 16 bytes after the message type and length (magic cookie + transaction id) for the
/// XOR- attributes, zeros for the plain ones.
fn decode_address(value: &[u8], mask: &[u8]) -> Option<SocketAddr> {
    let family = *value.get(1)?;
    let port = be16(value, 2)? ^ be16(mask, 0)?;
    let mut ip = [0u8; 16];
    let len = match (family, value.len()) {
        (0x01, 8) => 4,
        (0x02, 20) => 16,
        _ => return None,
    };
    for ((byte, v), m) in ip.iter_mut().zip(&value[4..]).zip(mask) {
        *byte = v ^ m;
    }
    let ip = match len {
        4 => IpAddr::from([ip[0], ip[1], ip[2], ip[3]]),
        _ => IpAddr::from(ip),
    };
    Some(SocketAddr::new(ip, port))
}

/// RFC 8489 §14.7: the CRC-32 of the message before FINGERPRINT (its length already counting it),
/// XOR 0x5354554E.
fn fingerprint_of(message: &[u8]) -> u32 {
    crc32(message) ^ FINGERPRINT_XOR
}

/// CRC-32 (ISO-HDLC: reflected 0x04C11DB7, as zlib), a bit at a time: no table to ship, and a STUN
/// message is a few dozen bytes.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

fn pad4(len: usize) -> usize {
    (len + 3) & !3
}

fn be16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

// ---- a Binding transaction ----------------------------------------------------------------------

/// The retransmission schedule (RFC 8489 §6.2.1): a request, then a retransmission after `rto`,
/// the next after twice that, and so on, `rc` requests in all; after the last one, `rm` × `rto`
/// for the response before giving up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub rto: Duration,
    /// Requests sent in all, the first one included (at least 1).
    pub rc: u32,
    pub rm: u32,
}

impl Timing {
    /// Sends at 0, 0.5, 1.5, 3.5, 7.5, 15.5, 31.5 s; gives up at 39.5 s.
    pub const RFC: Timing = Timing {
        rto: Duration::from_millis(500),
        rc: 7,
        rm: 16,
    };
}

impl Default for Timing {
    fn default() -> Timing {
        Timing::RFC
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    InProgress,
    /// The server-reflexive address.
    Done(SocketAddr),
    Failed(Reason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// No response within the [`Timing`].
    Timeout,
    /// An error response: 300 try alternate, 400 bad request, 401 unauthenticated, 420 unknown
    /// attribute, 438 stale nonce, 500 server error, ...
    Error { code: u16, phrase: String },
    /// A response to our request that cannot be used (RFC 8489 §6.3): a success response with no
    /// mapped address or with a comprehension-required attribute we do not know, an error response
    /// with no valid ERROR-CODE.
    BadResponse,
}

/// The comprehension-required attributes a Binding response may carry without failing: RFC 8489's
/// and RFC 3489's (RESPONSE-ADDRESS, CHANGE-REQUEST, SOURCE-ADDRESS, CHANGED-ADDRESS, PASSWORD,
/// REFLECTED-FROM — still sent by old servers).
const KNOWN_REQUIRED: &[u16] = &[
    attr::MAPPED_ADDRESS,
    0x0002,
    0x0003,
    0x0004,
    0x0005,
    attr::USERNAME,
    0x0007,
    attr::MESSAGE_INTEGRITY,
    attr::ERROR_CODE,
    attr::UNKNOWN_ATTRIBUTES,
    0x000B,
    attr::REALM,
    attr::NONCE,
    attr::MESSAGE_INTEGRITY_SHA256,
    attr::PASSWORD_ALGORITHM,
    attr::USERHASH,
    attr::XOR_MAPPED_ADDRESS,
];

/// One Binding transaction from one local socket to one server address: the request (with
/// FINGERPRINT, one transaction id for all its retransmissions) and the response.
#[derive(Debug)]
pub struct Binding {
    server: SocketAddr,
    id: TransactionId,
    request: Vec<u8>,
    timing: Timing,
    sent: u32,
    /// The next request; after the last one, the give-up.
    due: Instant,
    state: State,
}

impl Binding {
    pub fn new(server: SocketAddr, now: Instant) -> Binding {
        Binding::with_timing(server, now, Timing::RFC)
    }

    pub fn with_timing(server: SocketAddr, now: Instant, timing: Timing) -> Binding {
        Binding::with_id(server, now, timing, TransactionId::random(now))
    }

    fn with_id(server: SocketAddr, now: Instant, timing: Timing, id: TransactionId) -> Binding {
        let mut request = MessageWriter::new(BINDING, Class::Request, id);
        request.fingerprint();
        Binding {
            server,
            id,
            request: request.into_bytes(),
            timing: Timing {
                rc: timing.rc.max(1),
                ..timing
            },
            sent: 0,
            due: now,
            state: State::InProgress,
        }
    }

    pub fn server(&self) -> SocketAddr {
        self.server
    }

    pub fn state(&self) -> State {
        self.state.clone()
    }

    /// When to call [`Binding::poll_transmit`] next (the next request, or the give-up); None once
    /// finished.
    pub fn poll_timeout(&self) -> Option<Instant> {
        matches!(self.state, State::InProgress).then_some(self.due)
    }

    /// The request to send to the server now, if one is due. Past the last request's wait, this is
    /// where the transaction gives up (`Failed(Timeout)`).
    pub fn poll_transmit(&mut self, now: Instant) -> Option<&[u8]> {
        if !matches!(self.state, State::InProgress) || now < self.due {
            return None;
        }
        let Timing { rto, rc, rm } = self.timing;
        if self.sent == rc {
            self.state = State::Failed(Reason::Timeout);
            return None;
        }
        self.sent += 1;
        let wait = if self.sent < rc {
            rto.saturating_mul(1 << (self.sent - 1).min(31))
        } else {
            rto.saturating_mul(rm)
        };
        self.due = now.checked_add(wait).unwrap_or(now);
        Some(self.request.as_slice())
    }

    /// Offers a datagram that arrived on the socket: true = ours (a response from the server's
    /// address with our transaction id), and the caller drops it. Anything else returns false,
    /// changes nothing and goes to str0m. A response after the transaction finished (a duplicate
    /// from a retransmission) is consumed and changes nothing.
    pub fn handle_input(&mut self, from: SocketAddr, datagram: &[u8]) -> bool {
        if from.port() != self.server.port()
            || from.ip().to_canonical() != self.server.ip().to_canonical()
        {
            return false;
        }
        if datagram.get(8..HEADER_LEN) != Some(&self.id.0[..]) {
            return false;
        }
        let Ok(message) = Message::parse(datagram) else {
            return false;
        };
        let class = message.class();
        if message.method() != BINDING || !matches!(class, Class::Success | Class::Error) {
            return false;
        }
        if matches!(self.state, State::InProgress) {
            self.state = State::Failed(Reason::BadResponse);
            if class == Class::Error {
                if let Some((code, phrase)) = message.error_code() {
                    self.state = State::Failed(Reason::Error {
                        code,
                        phrase: phrase.to_owned(),
                    });
                }
            } else if message.unknown_required(KNOWN_REQUIRED).is_none() {
                let mapped = message.xor_address(attr::XOR_MAPPED_ADDRESS);
                if let Some(addr) = mapped.or_else(|| message.address(attr::MAPPED_ADDRESS)) {
                    self.state = State::Done(addr);
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 5769 §2.1-2.3, typed from the RFC's text.
    const RFC5769_ID: [u8; 12] = [
        0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
    ];

    #[rustfmt::skip]
    const RFC5769_REQUEST: [u8; 108] = [
        0x00, 0x01, 0x00, 0x58,
        0x21, 0x12, 0xa4, 0x42,
        0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
        0x80, 0x22, 0x00, 0x10, // SOFTWARE "STUN test client"
        0x53, 0x54, 0x55, 0x4e, 0x20, 0x74, 0x65, 0x73, 0x74, 0x20, 0x63, 0x6c, 0x69, 0x65, 0x6e, 0x74,
        0x00, 0x24, 0x00, 0x04, // PRIORITY
        0x6e, 0x00, 0x01, 0xff,
        0x80, 0x29, 0x00, 0x08, // ICE-CONTROLLED
        0x93, 0x2f, 0xf9, 0xb1, 0x51, 0x26, 0x3b, 0x36,
        0x00, 0x06, 0x00, 0x09, // USERNAME "evtj:h6vY" + padding
        0x65, 0x76, 0x74, 0x6a, 0x3a, 0x68, 0x36, 0x76, 0x59, 0x20, 0x20, 0x20,
        0x00, 0x08, 0x00, 0x14, // MESSAGE-INTEGRITY
        0x9a, 0xea, 0xa7, 0x0c, 0xbf, 0xd8, 0xcb, 0x56, 0x78, 0x1e, 0xf2, 0xb5, 0xb2, 0xd3, 0xf2, 0x49,
        0xc1, 0xb5, 0x71, 0xa2,
        0x80, 0x28, 0x00, 0x04, // FINGERPRINT
        0xe5, 0x7a, 0x3b, 0xcf,
    ];

    #[rustfmt::skip]
    const RFC5769_IPV4_RESPONSE: [u8; 80] = [
        0x01, 0x01, 0x00, 0x3c,
        0x21, 0x12, 0xa4, 0x42,
        0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
        0x80, 0x22, 0x00, 0x0b, // SOFTWARE "test vector" + padding
        0x74, 0x65, 0x73, 0x74, 0x20, 0x76, 0x65, 0x63, 0x74, 0x6f, 0x72, 0x20,
        0x00, 0x20, 0x00, 0x08, // XOR-MAPPED-ADDRESS
        0x00, 0x01, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43,
        0x00, 0x08, 0x00, 0x14, // MESSAGE-INTEGRITY
        0x2b, 0x91, 0xf5, 0x99, 0xfd, 0x9e, 0x90, 0xc3, 0x8c, 0x74, 0x89, 0xf9, 0x2a, 0xf9, 0xba, 0x53,
        0xf0, 0x6b, 0xe7, 0xd7,
        0x80, 0x28, 0x00, 0x04, // FINGERPRINT
        0xc0, 0x7d, 0x4c, 0x96,
    ];

    #[rustfmt::skip]
    const RFC5769_IPV6_RESPONSE: [u8; 92] = [
        0x01, 0x01, 0x00, 0x48,
        0x21, 0x12, 0xa4, 0x42,
        0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
        0x80, 0x22, 0x00, 0x0b, // SOFTWARE "test vector" + padding
        0x74, 0x65, 0x73, 0x74, 0x20, 0x76, 0x65, 0x63, 0x74, 0x6f, 0x72, 0x20,
        0x00, 0x20, 0x00, 0x14, // XOR-MAPPED-ADDRESS
        0x00, 0x02, 0xa1, 0x47,
        0x01, 0x13, 0xa9, 0xfa, 0xa5, 0xd3, 0xf1, 0x79, 0xbc, 0x25, 0xf4, 0xb5, 0xbe, 0xd2, 0xb9, 0xd9,
        0x00, 0x08, 0x00, 0x14, // MESSAGE-INTEGRITY
        0xa3, 0x82, 0x95, 0x4e, 0x4b, 0xe6, 0x7b, 0xf1, 0x17, 0x84, 0xc9, 0x7c, 0x82, 0x92, 0xc2, 0x75,
        0xbf, 0xe3, 0xed, 0x41,
        0x80, 0x28, 0x00, 0x04, // FINGERPRINT
        0xc8, 0xfb, 0x0b, 0x4c,
    ];

    fn server() -> SocketAddr {
        "198.51.100.7:3478".parse().unwrap()
    }

    fn mapped() -> SocketAddr {
        "203.0.113.9:61000".parse().unwrap()
    }

    fn binding(id: [u8; 12]) -> (Binding, Instant) {
        let t0 = Instant::now();
        (
            Binding::with_id(server(), t0, Timing::RFC, TransactionId(id)),
            t0,
        )
    }

    fn success(id: [u8; 12], addr: SocketAddr) -> Vec<u8> {
        let mut w = MessageWriter::new(BINDING, Class::Success, TransactionId(id));
        w.xor_address(attr::XOR_MAPPED_ADDRESS, addr).fingerprint();
        w.into_bytes()
    }

    fn error_code(code: u16, phrase: &str) -> Vec<u8> {
        let mut v = vec![0, 0, (code / 100) as u8, (code % 100) as u8];
        v.extend_from_slice(phrase.as_bytes());
        v
    }

    const ID: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];

    #[test]
    fn stun_crc32_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn stun_rfc5769_ipv4_response() {
        let m = Message::parse(&RFC5769_IPV4_RESPONSE).expect("parses, FINGERPRINT verifies");
        assert_eq!(m.class(), Class::Success);
        assert_eq!(m.method(), BINDING);
        assert_eq!(m.transaction_id(), TransactionId(RFC5769_ID));
        assert_eq!(m.get(attr::SOFTWARE), Some(&b"test vector"[..]));
        assert_eq!(
            m.xor_address(attr::XOR_MAPPED_ADDRESS),
            Some("192.0.2.1:32853".parse().unwrap())
        );
        assert_eq!(m.attributes().count(), 4);
        let crc = u32::from_be_bytes(RFC5769_IPV4_RESPONSE[76..80].try_into().unwrap());
        assert_eq!(fingerprint_of(&RFC5769_IPV4_RESPONSE[..72]), crc);

        let (mut b, _) = binding(RFC5769_ID);
        assert!(b.handle_input(server(), &RFC5769_IPV4_RESPONSE));
        assert_eq!(b.state(), State::Done("192.0.2.1:32853".parse().unwrap()));

        // One flipped bit before FINGERPRINT (here in MESSAGE-INTEGRITY): not a message.
        let mut bad = RFC5769_IPV4_RESPONSE;
        bad[60] ^= 0x01;
        assert_eq!(Message::parse(&bad).unwrap_err(), ParseError::Fingerprint);
        let (mut b, _) = binding(RFC5769_ID);
        assert!(!b.handle_input(server(), &bad));
        assert_eq!(b.state(), State::InProgress);
    }

    #[test]
    fn stun_rfc5769_ipv6_response() {
        let m = Message::parse(&RFC5769_IPV6_RESPONSE).expect("parses, FINGERPRINT verifies");
        assert_eq!(m.class(), Class::Success);
        assert_eq!(m.transaction_id(), TransactionId(RFC5769_ID));
        let want: SocketAddr = "[2001:db8:1234:5678:11:2233:4455:6677]:32853"
            .parse()
            .unwrap();
        assert_eq!(m.xor_address(attr::XOR_MAPPED_ADDRESS), Some(want));
        let crc = u32::from_be_bytes(RFC5769_IPV6_RESPONSE[88..92].try_into().unwrap());
        assert_eq!(fingerprint_of(&RFC5769_IPV6_RESPONSE[..84]), crc);

        let (mut b, _) = binding(RFC5769_ID);
        assert!(b.handle_input(server(), &RFC5769_IPV6_RESPONSE));
        assert_eq!(b.state(), State::Done(want));
    }

    #[test]
    fn stun_rfc5769_request_is_a_request_not_ours() {
        let m = Message::parse(&RFC5769_REQUEST).expect("parses, FINGERPRINT verifies");
        assert_eq!(m.class(), Class::Request);
        assert_eq!(m.method(), BINDING);
        assert_eq!(m.get(attr::USERNAME), Some(&b"evtj:h6vY"[..]));
        // An ICE check on the socket is not ours, even with our id and from the server's address.
        let (mut b, _) = binding(RFC5769_ID);
        assert!(!b.handle_input(server(), &RFC5769_REQUEST));
        assert_eq!(b.state(), State::InProgress);
    }

    #[test]
    fn stun_our_request_parses_back_with_fingerprint() {
        let t0 = Instant::now();
        let mut b = Binding::new(server(), t0);
        let id = b.id;
        let request = b.poll_transmit(t0).expect("due at once").to_vec();
        assert_eq!(request.len(), 28);
        assert!(is_stun(&request));
        let m = Message::parse(&request).unwrap();
        assert_eq!(
            (m.class(), m.method(), m.transaction_id()),
            (Class::Request, BINDING, id)
        );
        let attrs: Vec<_> = m.attributes().collect();
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0].0, attr::FINGERPRINT);
        assert_eq!(
            attrs[0].1,
            &(crc32(&request[..20]) ^ 0x5354_554E).to_be_bytes()[..]
        );
        assert_eq!(&request[..4], &[0x00, 0x01, 0x00, 0x08]);
    }

    #[test]
    fn stun_transaction_ids_differ() {
        let now = Instant::now();
        let ids: std::collections::HashSet<_> =
            (0..1000).map(|_| TransactionId::random(now)).collect();
        assert_eq!(ids.len(), 1000);
    }

    fn sends(b: &mut Binding, t0: Instant, until_ms: u64) -> Vec<u64> {
        // Polls at every millisecond: the instants a request came out at.
        let mut at = Vec::new();
        let mut first = None;
        for ms in 0..=until_ms {
            if let Some(bytes) = b.poll_transmit(t0 + Duration::from_millis(ms)) {
                assert_eq!(
                    *first.get_or_insert_with(|| bytes.to_vec()),
                    bytes,
                    "retransmissions are the same bytes"
                );
                at.push(ms);
            }
        }
        at
    }

    #[test]
    fn stun_retransmission_schedule_rfc() {
        let (mut b, t0) = binding(ID);
        let ms = |ms: u64| t0 + Duration::from_millis(ms);
        assert_eq!(b.poll_timeout(), Some(t0));
        assert_eq!(
            sends(&mut b, t0, 31_500),
            [0, 500, 1500, 3500, 7500, 15_500, 31_500]
        );
        assert_eq!(b.poll_timeout(), Some(ms(39_500)));
        assert_eq!(b.poll_transmit(ms(39_499)), None);
        assert_eq!(b.state(), State::InProgress);
        assert_eq!(b.poll_transmit(ms(39_500)), None);
        assert_eq!(b.state(), State::Failed(Reason::Timeout));
        assert_eq!(b.poll_timeout(), None);
        assert_eq!(b.poll_transmit(ms(100_000)), None);
        // A late response is still consumed (it is ours) but the result stands.
        assert!(b.handle_input(server(), &success(ID, mapped())));
        assert_eq!(b.state(), State::Failed(Reason::Timeout));
    }

    #[test]
    fn stun_retransmission_schedule_tunable() {
        let t0 = Instant::now();
        let timing = Timing {
            rto: Duration::from_millis(100),
            rc: 3,
            rm: 4,
        };
        let mut b = Binding::with_timing(server(), t0, timing);
        assert_eq!(sends(&mut b, t0, 699), [0, 100, 300]);
        assert_eq!(b.state(), State::InProgress);
        assert_eq!(b.poll_timeout(), Some(t0 + Duration::from_millis(700)));
        b.poll_transmit(t0 + Duration::from_millis(700));
        assert_eq!(b.state(), State::Failed(Reason::Timeout));

        // rc = 1: one request, then rm × rto.
        let mut b = Binding::with_timing(
            server(),
            t0,
            Timing {
                rto: Duration::from_millis(250),
                rc: 1,
                rm: 2,
            },
        );
        assert_eq!(sends(&mut b, t0, 499), [0]);
        b.poll_transmit(t0 + Duration::from_millis(500));
        assert_eq!(b.state(), State::Failed(Reason::Timeout));

        // A late poll: the next interval counts from the send that happened.
        let mut b = Binding::with_timing(server(), t0, timing);
        assert!(b.poll_transmit(t0 + Duration::from_millis(40)).is_some());
        assert_eq!(b.poll_timeout(), Some(t0 + Duration::from_millis(140)));
    }

    #[test]
    fn stun_success_finishes_with_the_reflexive_address() {
        let (mut b, t0) = binding(ID);
        b.poll_transmit(t0);
        assert!(b.handle_input(server(), &success(ID, mapped())));
        assert_eq!(b.state(), State::Done(mapped()));
        assert_eq!(b.poll_timeout(), None);
        assert_eq!(b.poll_transmit(t0 + Duration::from_secs(1)), None);
        // A duplicate (or a second, different answer) is consumed; the first result stands.
        assert!(b.handle_input(server(), &success(ID, "192.0.2.200:1".parse().unwrap())));
        assert_eq!(b.state(), State::Done(mapped()));

        // IPv6, and the old MAPPED-ADDRESS alone.
        let v6: SocketAddr = "[2001:db8::42]:50000".parse().unwrap();
        let (mut b, _) = binding(ID);
        assert!(b.handle_input(server(), &success(ID, v6)));
        assert_eq!(b.state(), State::Done(v6));
        let mut w = MessageWriter::new(BINDING, Class::Success, TransactionId(ID));
        w.attribute(attr::MAPPED_ADDRESS, &[0, 1, 0xEE, 0x48, 203, 0, 113, 9]);
        let (mut b, _) = binding(ID);
        assert!(b.handle_input(server(), &w.into_bytes()));
        assert_eq!(b.state(), State::Done(mapped()));
    }

    #[test]
    fn stun_error_response_fails_with_the_code() {
        let mut w = MessageWriter::new(BINDING, Class::Error, TransactionId(ID));
        w.attribute(attr::ERROR_CODE, &error_code(401, "Unauthorized"));
        w.attribute(attr::REALM, b"example.org")
            .attribute(attr::NONCE, b"f//499k954d6OL34oL9FSTvy64sA")
            .fingerprint();
        let (mut b, t0) = binding(ID);
        b.poll_transmit(t0);
        assert!(b.handle_input(server(), &w.into_bytes()));
        assert_eq!(
            b.state(),
            State::Failed(Reason::Error {
                code: 401,
                phrase: "Unauthorized".into()
            })
        );
        assert_eq!(b.poll_timeout(), None);
    }

    #[test]
    fn stun_unusable_responses_fail_the_transaction() {
        type Case = (&'static str, fn(&mut MessageWriter), State);
        let cases: [Case; 5] = [
            (
                "success without an address",
                |_| {},
                State::Failed(Reason::BadResponse),
            ),
            (
                "unknown comprehension-required",
                |w| {
                    w.xor_address(attr::XOR_MAPPED_ADDRESS, mapped())
                        .attribute(0x7123, b"?");
                },
                State::Failed(Reason::BadResponse),
            ),
            (
                "unknown comprehension-optional is skipped",
                |w| {
                    w.attribute(0x8123, b"skip me")
                        .xor_address(attr::XOR_MAPPED_ADDRESS, mapped());
                },
                State::Done(mapped()),
            ),
            (
                "an unknown required one after MESSAGE-INTEGRITY is ignored",
                |w| {
                    w.xor_address(attr::XOR_MAPPED_ADDRESS, mapped())
                        .attribute(attr::MESSAGE_INTEGRITY, &[0; 20]);
                    w.attribute(0x7123, b"?");
                },
                State::Done(mapped()),
            ),
            (
                "a malformed XOR-MAPPED-ADDRESS",
                |w| {
                    w.attribute(attr::XOR_MAPPED_ADDRESS, &[0, 1, 0, 0, 1, 2, 3]);
                },
                State::Failed(Reason::BadResponse),
            ),
        ];
        for (what, build, want) in cases {
            let mut w = MessageWriter::new(BINDING, Class::Success, TransactionId(ID));
            build(&mut w);
            w.fingerprint();
            let (mut b, _) = binding(ID);
            assert!(b.handle_input(server(), &w.into_bytes()), "{what}");
            assert_eq!(b.state(), want, "{what}");
        }
        let values: [&[u8]; 4] = [&[], &[0, 0, 2, 0], &[0, 0, 4, 100], &[0, 0, 7, 0]];
        for value in values {
            let mut w = MessageWriter::new(BINDING, Class::Error, TransactionId(ID));
            if !value.is_empty() {
                w.attribute(attr::ERROR_CODE, value);
            }
            let (mut b, _) = binding(ID);
            assert!(b.handle_input(server(), &w.into_bytes()));
            assert_eq!(
                b.state(),
                State::Failed(Reason::BadResponse),
                "ERROR-CODE {value:?}"
            );
        }
        // A phrase that stops being UTF-8 is cut there.
        let mut value = error_code(500, "Server");
        value.extend_from_slice(&[0xFF, b'x']);
        let mut w = MessageWriter::new(BINDING, Class::Error, TransactionId(ID));
        w.attribute(attr::ERROR_CODE, &value);
        let (mut b, _) = binding(ID);
        assert!(b.handle_input(server(), &w.into_bytes()));
        assert_eq!(
            b.state(),
            State::Failed(Reason::Error {
                code: 500,
                phrase: "Server".into()
            })
        );
    }

    #[test]
    fn stun_demultiplexing_is_exact() {
        let ours = success(ID, mapped());
        let (mut b, t0) = binding(ID);
        b.poll_transmit(t0);

        // The right id from the wrong address: another port, another host.
        assert!(!b.handle_input("198.51.100.7:3479".parse().unwrap(), &ours));
        assert!(!b.handle_input("198.51.100.8:3478".parse().unwrap(), &ours));
        // The wrong id from the right address (str0m's transaction, say).
        let mut other = ID;
        other[11] ^= 0x80;
        assert!(!b.handle_input(server(), &success(other, mapped())));
        // Not STUN: RTP, RTCP, DTLS, empty, a header with no magic cookie, a TURN ChannelData.
        let mut rtp = vec![
            0x80, 0x6f, 0x12, 0x34, 0, 0, 0x10, 0, 0xde, 0xad, 0xbe, 0xef,
        ];
        rtp.extend_from_slice(&[0x55; 160]);
        let mut rtcp = vec![0x81, 0xc9, 0x00, 0x07, 0xde, 0xad, 0xbe, 0xef];
        rtcp.resize(32, 0);
        let mut dtls = vec![0x16, 0xfe, 0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x30, 0x01];
        dtls.extend_from_slice(&[0; 48]);
        let mut no_cookie = ours.clone();
        no_cookie[4] ^= 1;
        let mut channel_data = ours.clone();
        channel_data[0] |= 0x40;
        for datagram in [
            &rtp[..],
            &rtcp[..],
            &dtls[..],
            &[][..],
            &no_cookie[..],
            &channel_data[..],
        ] {
            assert!(!b.handle_input(server(), datagram), "{datagram:02x?}");
        }
        assert!(!is_stun(&rtp) && !is_stun(&dtls) && !is_stun(&no_cookie) && is_stun(&ours));
        // STUN with our id from the server, but not a response to our request: a request (an ICE
        // check), an indication, another method, a datagram with trailing bytes.
        let mut w = MessageWriter::new(BINDING, Class::Request, TransactionId(ID));
        w.fingerprint();
        assert!(!b.handle_input(server(), &w.into_bytes()));
        let mut w = MessageWriter::new(BINDING, Class::Indication, TransactionId(ID));
        w.fingerprint();
        assert!(!b.handle_input(server(), &w.into_bytes()));
        let mut w = MessageWriter::new(0x003, Class::Success, TransactionId(ID));
        w.xor_address(attr::XOR_MAPPED_ADDRESS, mapped())
            .fingerprint();
        assert!(!b.handle_input(server(), &w.into_bytes()));
        let mut trailing = ours.clone();
        trailing.extend_from_slice(&[0; 4]);
        assert!(!b.handle_input(server(), &trailing));
        assert_eq!(
            b.state(),
            State::InProgress,
            "nothing above changed anything"
        );

        // The server's address as an IPv4-mapped IPv6 one (a dual-stack socket) is the server.
        assert!(b.handle_input("[::ffff:198.51.100.7]:3478".parse().unwrap(), &ours));
        assert_eq!(b.state(), State::Done(mapped()));
    }

    #[test]
    fn stun_message_type_round_trips() {
        for method in [BINDING, 0x003, 0x009, 0xFFF] {
            for class in [
                Class::Request,
                Class::Indication,
                Class::Success,
                Class::Error,
            ] {
                let bytes = MessageWriter::new(method, class, TransactionId(ID)).into_bytes();
                let m = Message::parse(&bytes).unwrap();
                assert_eq!((m.method(), m.class()), (method, class));
            }
        }
        let bytes = MessageWriter::new(BINDING, Class::Error, TransactionId(ID)).into_bytes();
        assert_eq!(&bytes[..2], &[0x01, 0x11]);
    }

    /// xorshift64*: deterministic noise.
    struct Noise(u64);

    impl Noise {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Everything that reads a message, on anything: none of it may panic.
    fn poke(datagram: &[u8]) {
        let _ = is_stun(datagram);
        if let Ok(m) = Message::parse(datagram) {
            let _ = (
                m.class(),
                m.method(),
                m.transaction_id(),
                m.attributes().count(),
            );
            for ty in [
                attr::XOR_MAPPED_ADDRESS,
                attr::MAPPED_ADDRESS,
                attr::ERROR_CODE,
                attr::SOFTWARE,
            ] {
                let _ = (m.get(ty), m.address(ty), m.xor_address(ty));
            }
            let _ = (m.error_code(), m.unknown_required(KNOWN_REQUIRED));
        }
        let (mut b, _) = binding(ID);
        let _ = b.handle_input(server(), datagram);
        let _ = b.state();
    }

    #[test]
    fn stun_truncations_never_panic_and_are_not_consumed() {
        let mut w = MessageWriter::new(BINDING, Class::Error, TransactionId(ID));
        w.attribute(attr::ERROR_CODE, &error_code(438, "Stale Nonce"))
            .fingerprint();
        let samples = [
            success(ID, mapped()),
            success(ID, "[2001:db8::1]:9".parse().unwrap()),
            w.into_bytes(),
            RFC5769_IPV4_RESPONSE.to_vec(),
            RFC5769_IPV6_RESPONSE.to_vec(),
            RFC5769_REQUEST.to_vec(),
        ];
        for sample in &samples {
            assert!(Message::parse(sample).is_ok());
            for len in 0..sample.len() {
                let cut = &sample[..len];
                poke(cut);
                assert!(Message::parse(cut).is_err(), "a cut at {len} parses");
                let (mut b, _) = binding(ID);
                assert!(!b.handle_input(server(), cut));
                // With the header's length fixed up to the cut, the framing still has to hold.
                if len >= HEADER_LEN {
                    let mut fixed = cut.to_vec();
                    fixed[2..4].copy_from_slice(&((len - HEADER_LEN) as u16).to_be_bytes());
                    poke(&fixed);
                }
            }
        }
    }

    #[test]
    fn stun_noise_never_panics() {
        let mut noise = Noise(0x9E37_79B9_7F4A_7C15);
        let base = success(ID, mapped());
        let mut parsed = 0;
        for round in 0..6000 {
            let datagram = match round % 3 {
                // Raw noise.
                0 => (0..noise.below(120))
                    .map(|_| noise.next() as u8)
                    .collect::<Vec<_>>(),
                // A valid header with our id, the length field right, attributes of noise.
                1 => {
                    let body = 4 * noise.below(24);
                    let mut d =
                        MessageWriter::new(BINDING, Class::Success, TransactionId(ID)).into_bytes();
                    d[2..4].copy_from_slice(&(body as u16).to_be_bytes());
                    d.extend((0..body).map(|_| noise.next() as u8));
                    d
                }
                // A well-framed message with FINGERPRINT whose attributes are noise: reaches every
                // value decoder through a parse that succeeds.
                _ => {
                    let class = [Class::Success, Class::Error][noise.below(2)];
                    let mut w = MessageWriter::new(BINDING, class, TransactionId(ID));
                    for _ in 0..noise.below(5) {
                        // Any type but FINGERPRINT, which may only come last.
                        let any = match noise.next() as u16 {
                            attr::FINGERPRINT => 0x8029,
                            ty => ty,
                        };
                        let ty = [
                            attr::XOR_MAPPED_ADDRESS,
                            attr::MAPPED_ADDRESS,
                            attr::ERROR_CODE,
                            attr::MESSAGE_INTEGRITY,
                            any,
                        ][noise.below(5)];
                        let value: Vec<u8> =
                            (0..noise.below(24)).map(|_| noise.next() as u8).collect();
                        w.attribute(ty, &value);
                    }
                    w.fingerprint();
                    let d = w.into_bytes();
                    parsed += Message::parse(&d).is_ok() as u32;
                    d
                }
            };
            poke(&datagram);
            // And the valid response with a few bytes changed.
            let mut mutated = base.clone();
            for _ in 0..1 + noise.below(3) {
                let at = noise.below(mutated.len());
                mutated[at] = noise.next() as u8;
            }
            poke(&mutated);
        }
        assert_eq!(parsed, 2000, "the writer's messages all parse");
    }

    #[test]
    fn stun_ice_urls() {
        use Scheme::{Stun, Turn, Turns};
        use Transport::{Tcp, Udp};
        for (good, (scheme, host, port, transport)) in [
            (
                "stun:stun.l.google.com:19302",
                (Stun, "stun.l.google.com", 19302, Udp),
            ),
            ("stun:example.org", (Stun, "example.org", 3478, Udp)),
            ("STUN:Example.ORG", (Stun, "Example.ORG", 3478, Udp)),
            ("stun:192.0.2.1:3479", (Stun, "192.0.2.1", 3479, Udp)),
            ("stun:[2001:db8::1]:3478", (Stun, "2001:db8::1", 3478, Udp)),
            ("stun:[::1]", (Stun, "::1", 3478, Udp)),
            (
                "stun:[::ffff:192.0.2.1]:1",
                (Stun, "::ffff:192.0.2.1", 1, Udp),
            ),
            ("turn:t.example.org", (Turn, "t.example.org", 3478, Udp)),
            (
                "turn:t.example.org:3480?transport=udp",
                (Turn, "t.example.org", 3480, Udp),
            ),
            (
                "turn:t.example.org?transport=tcp",
                (Turn, "t.example.org", 3478, Tcp),
            ),
            (
                "turn:[2001:db8::2]:443?Transport=TCP",
                (Turn, "2001:db8::2", 443, Tcp),
            ),
            ("turns:t.example.org", (Turns, "t.example.org", 5349, Tcp)),
            (
                "turns:t.example.org:443?transport=tcp",
                (Turns, "t.example.org", 443, Tcp),
            ),
            ("turns:[2001:db8::3]", (Turns, "2001:db8::3", 5349, Tcp)),
        ] {
            let host = host.to_owned();
            let want = IceUrl {
                scheme,
                host,
                port,
                transport,
            };
            assert_eq!(parse_ice_url(good), Ok(want), "{good:?}");
        }

        use UrlError as E;
        for (bad, why) in [
            ("", E::Scheme),
            ("stun", E::Scheme),
            ("http:example.org", E::Scheme),
            ("stuns:example.org", E::Scheme),
            (" stun:example.org", E::Scheme),
            ("stun:", E::Host),
            ("stun://example.org", E::Host),
            ("stun:user@example.org", E::Host),
            ("stun:exa mple.org", E::Host),
            ("stun:example.org/", E::Host),
            ("stun:2001:db8::1", E::Host),
            ("stun:::1", E::Host),
            ("stun:[2001:db8::1", E::Host),
            ("stun:[2001:db8::1]x", E::Host),
            ("stun:[2001:db8::1]3478", E::Host),
            ("stun:[1.2.3.4]", E::Host),
            ("stun:[fe80::1%25eth0]", E::Host),
            ("stun:[]", E::Host),
            ("stun:example.org:", E::Port),
            ("stun:example.org:0", E::Port),
            ("stun:example.org:65536", E::Port),
            ("stun:example.org:+1", E::Port),
            ("stun:example.org:3a", E::Port),
            ("stun:[::1]:", E::Port),
            ("stun:example.org?transport=udp", E::Query),
            ("turn:example.org?", E::Query),
            ("turn:example.org?transport=sctp", E::Query),
            ("turn:example.org?foo=udp", E::Query),
            ("turn:example.org?transport=udp&x=1", E::Query),
            ("turns:example.org?transport=udp", E::Query),
        ] {
            assert_eq!(parse_ice_url(bad), Err(why), "{bad:?}");
        }
    }

    /// The real thing: a Binding over a real UDP socket to Google's public STUN server.
    #[test]
    #[ignore = "network"]
    fn stun_binding_against_a_public_server() {
        use std::io::ErrorKind;
        use std::net::{ToSocketAddrs, UdpSocket};
        let url = parse_ice_url("stun:stun.l.google.com:19302").unwrap();
        let server = (url.host.as_str(), url.port)
            .to_socket_addrs()
            .unwrap()
            .find(SocketAddr::is_ipv4)
            .expect("an IPv4 address");
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        let timing = Timing {
            rto: Duration::from_millis(250),
            rc: 5,
            rm: 8,
        };
        let mut b = Binding::with_timing(server, Instant::now(), timing);
        let mut buf = [0u8; 1500];
        let mut sent = 0;
        while let Some(due) = b.poll_timeout() {
            let now = Instant::now();
            if now >= due {
                if let Some(request) = b.poll_transmit(now) {
                    socket.send_to(request, server).unwrap();
                    sent += 1;
                }
                continue;
            }
            socket.set_read_timeout(Some(due - now)).unwrap();
            match socket.recv_from(&mut buf) {
                Ok((n, from)) => {
                    let consumed = b.handle_input(from, &buf[..n]);
                    println!("{n} bytes from {from}: consumed = {consumed}");
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::ConnectionReset
                    ) => {}
                Err(e) => panic!("{e}"),
            }
        }
        println!(
            "server {server}, local {}, {sent} request(s): {:?}",
            socket.local_addr().unwrap(),
            b.state()
        );
        assert!(matches!(b.state(), State::Done(_)));
    }
}
