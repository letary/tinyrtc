//! The network thread: ONE thread owns the UDP socket, every `Rtc` of every room and the call
//! table. Its loop waits for one thing — a datagram on the socket, with the earliest deadline
//! (a str0m timeout, the housekeeping) as the read timeout — and applies exactly one input: a
//! command of the API, a due timer, a datagram. Every input that mutates an `Rtc` is followed by
//! polling that `Rtc` dry (str0m's rule, core/src/connection.rs), and what the poll brings out for
//! OTHER connections (media to forward, a keyframe to ask for, a close) is queued as `Work` and
//! applied after, one mutation and one poll at a time.
//!
//! The API's commands come through a channel; so that the loop needs no second thing to wait on,
//! whoever sends one also sends a one-byte datagram from the `waker` socket (loopback, its address
//! known) to the server's own socket. A datagram from the waker means only "look at the channel";
//! the channel is also looked at on every wake, so a lost wake-up costs latency, not a command.
//! Neither send blocks, so the API's async tasks make them directly; each command carries a
//! oneshot sender, the reply the task awaits.
//!
//! Demultiplexing, in this order:
//!   (a) a source address already latched: its room connection or its call side;
//!   (b) a STUN binding request: a room connection whose ICE ufrag is the first half of its
//!       USERNAME (and whose `rtc.accepts` agrees), else a call reserved for the pair;
//!   (c) anything else from an unknown address: dropped.
//! A binding request from a latched address whose USERNAME belongs to someone else (a NAT that
//! reused a port mapping) goes through (b) as if the address were unknown.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use str0m::change::SdpOffer;
use str0m::config::DtlsCert;
use str0m::net::{Protocol, Receive};
use str0m::{Input, Output};
use tinyrtc::stun;
use tokio::sync::oneshot;

use crate::call::Call;
use crate::room::{Conn, Room, Work};
use crate::{Config, Log};

pub(crate) type ConnId = u64;
/// A room's or a call's id; the API shows it as 16 hex digits.
pub(crate) type Id = u64;

/// Who a latched address belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Owner {
    Conn(ConnId),
    Call(Id, usize),
}

/// What the API gets back.
pub(crate) enum Body {
    Json(String),
    Sdp(String),
    Empty,
}

#[derive(Debug)]
pub(crate) struct ApiError {
    pub status: u16,
    pub message: String,
}

impl ApiError {
    pub(crate) fn new(status: u16, message: impl Into<String>) -> ApiError {
        ApiError { status, message: message.into() }
    }
}

pub(crate) type Response = Result<(u16, Body), ApiError>;
pub(crate) type Reply = oneshot::Sender<Response>;

pub(crate) enum Command {
    CreateCall { ufrags: [String; 2], reply: Reply },
    GetCall { id: Id, reply: Reply },
    DeleteCall { id: Id, reply: Reply },
    CreateRoom { reply: Reply },
    GetRoom { id: Id, reply: Reply },
    DeleteRoom { id: Id, reply: Reply },
    Publish { room: Id, user: String, offer: Box<SdpOffer>, reply: Reply },
    Subscribe { room: Id, user: String, other: String, offer: Box<SdpOffer>, reply: Reply },
    DeleteParticipant { room: Id, user: String, reply: Reply },
    Stop,
}

/// The API's side: a sender into the network thread's queue, and its wake-up.
pub(crate) struct Handle {
    tx: Sender<Command>,
    waker: UdpSocket,
    target: SocketAddr,
}

impl Handle {
    /// Queues a command; its reply comes on the receiver. None = the network thread is gone.
    pub(crate) fn ask(&self, make: impl FnOnce(Reply) -> Command) -> Option<oneshot::Receiver<Response>> {
        let (reply, answer) = oneshot::channel();
        self.tx.send(make(reply)).ok()?;
        let _ = self.waker.send_to(&[0], self.target);
        Some(answer)
    }

    pub(crate) fn stop(&self) {
        let _ = self.tx.send(Command::Stop);
        let _ = self.waker.send_to(&[0], self.target);
    }
}

/// The idle timers are looked at this often (or more often, for short timeouts).
const HOUSEKEEPING: Duration = Duration::from_millis(250);
/// A room connection remembers at most this many source addresses (a phone's checks come from
/// each of its candidates).
const MAX_ADDRS: usize = 8;
/// The largest datagram read whole.
const DATAGRAM: usize = 65_536;

pub(crate) struct Net {
    pub(crate) socket: UdpSocket,
    /// The candidate address: the public IP with the bound port. str0m is told every datagram
    /// arrived on it.
    pub(crate) public: SocketAddr,
    waker: SocketAddr,
    commands: Receiver<Command>,
    pub(crate) log: Log,

    pub(crate) connection_idle: Duration,
    pub(crate) room_empty: Duration,
    pub(crate) call_idle: Duration,
    pub(crate) keyframe_interval: Duration,
    pub(crate) subscriber_bitrate: u64,
    housekeeping: Duration,
    housekeeping_at: Instant,
    /// Every room connection's DTLS identity: made once, not per connection.
    pub(crate) cert: DtlsCert,
    /// The `candidate:` line both phones of a call are given.
    pub(crate) call_candidate: String,

    pub(crate) rooms: HashMap<Id, Room>,
    pub(crate) conns: HashMap<ConnId, Conn>,
    /// Our ICE ufrag of each room connection: the first half of a USERNAME sent to it.
    pub(crate) by_ufrag: HashMap<String, ConnId>,
    pub(crate) next_conn: ConnId,
    pub(crate) calls: HashMap<Id, Call>,
    /// "<receiver ufrag>:<sender ufrag>" -> the call and the SENDER's side.
    pub(crate) by_username: HashMap<String, (Id, usize)>,
    pub(crate) owners: HashMap<SocketAddr, Owner>,
    /// (str0m's next timeout, connection), one entry per connection.
    pub(crate) timers: BTreeSet<(Instant, ConnId)>,
    pub(crate) work: VecDeque<Work>,
    /// Publish connections owed a keyframe request (rate-limited, or not ready yet).
    pub(crate) keyframes: BTreeSet<ConnId>,
    /// The connection whose `Rtc` is being touched: the one to drop if it panics.
    pub(crate) touching: Option<ConnId>,
}

impl Net {
    pub(crate) fn new(socket: UdpSocket, public: SocketAddr, config: &Config, log: Log) -> std::io::Result<(Handle, Net)> {
        let local = socket.local_addr()?;
        let target = match local.ip() {
            IpAddr::V4(ip) if ip.is_unspecified() => SocketAddr::new(Ipv4Addr::LOCALHOST.into(), local.port()),
            IpAddr::V6(ip) if ip.is_unspecified() => SocketAddr::new(Ipv6Addr::LOCALHOST.into(), local.port()),
            _ => local,
        };
        let waker = UdpSocket::bind(SocketAddr::new(target.ip(), 0))?;
        let waker_addr = waker.local_addr()?;
        let cert = str0m::crypto::from_feature_flags()
            .dtls_provider
            .generate_certificate()
            .ok_or_else(|| std::io::Error::other("no DTLS certificate"))?;
        let (tx, commands) = mpsc::channel();
        let shortest = config.connection_idle.min(config.call_idle).min(config.room_empty);
        let housekeeping = (shortest / 8).clamp(Duration::from_millis(10), HOUSEKEEPING);
        let net = Net {
            socket,
            public,
            waker: waker_addr,
            commands,
            log,
            connection_idle: config.connection_idle,
            room_empty: config.room_empty,
            call_idle: config.call_idle,
            keyframe_interval: config.keyframe_interval,
            subscriber_bitrate: config.subscriber_bitrate,
            housekeeping,
            housekeeping_at: Instant::now() + housekeeping,
            cert,
            call_candidate: crate::call::relay_candidate(public),
            rooms: HashMap::new(),
            conns: HashMap::new(),
            by_ufrag: HashMap::new(),
            next_conn: 1,
            calls: HashMap::new(),
            by_username: HashMap::new(),
            owners: HashMap::new(),
            timers: BTreeSet::new(),
            work: VecDeque::new(),
            keyframes: BTreeSet::new(),
            touching: None,
        };
        Ok((Handle { tx, waker, target }, net))
    }

    pub(crate) fn run(mut self) {
        let mut buf = vec![0u8; DATAGRAM];
        let mut error_logged: Option<Instant> = None;
        loop {
            loop {
                match self.commands.try_recv() {
                    Ok(Command::Stop) | Err(TryRecvError::Disconnected) => return self.shutdown(),
                    Ok(command) => self.guarded(|net| net.command(command)),
                    Err(TryRecvError::Empty) => break,
                }
            }
            let now = Instant::now();
            self.guarded(|net| net.timers_due(now));
            if now >= self.housekeeping_at {
                self.housekeeping_at = now + self.housekeeping;
                self.guarded(|net| net.housekeeping(now));
            }

            let next = self.timers.first().map_or(self.housekeeping_at, |(at, _)| (*at).min(self.housekeeping_at));
            // A read timeout of zero is "no timeout" to the OS: at least a millisecond.
            let wait = next.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
            let _ = self.socket.set_read_timeout(Some(wait));
            match self.socket.recv_from(&mut buf) {
                Ok((_, from)) if from == self.waker => {}
                Ok((n, from)) => {
                    let now = Instant::now();
                    self.guarded(|net| net.datagram(from, &buf[..n], now));
                }
                Err(e) => match e.kind() {
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted => {}
                    // Windows reports an ICMP "port unreachable" of an earlier send here; a
                    // datagram larger than the buffer too.
                    ErrorKind::ConnectionReset | ErrorKind::ConnectionRefused => {}
                    _ => {
                        // At most a line a second of an error that does not go away.
                        if error_logged.is_none_or(|at| at.elapsed() >= Duration::from_secs(1)) {
                            log_line!(self.log, WARN, "udp receive error: {e}");
                            error_logged = Some(Instant::now());
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                },
            }
        }
    }

    /// One input, with a panic (str0m's or ours) costing the connection being touched, not the
    /// thread every room and call lives on.
    fn guarded(&mut self, f: impl FnOnce(&mut Net)) {
        self.touching = None;
        if catch_unwind(AssertUnwindSafe(|| f(self))).is_ok() {
            return;
        }
        self.work.clear();
        match self.touching.take() {
            Some(id) => {
                log_line!(self.log, ERROR, "panic while handling connection {id}: dropped");
                self.discard(id);
            }
            None => log_line!(self.log, ERROR, "panic outside any connection"),
        }
    }

    fn shutdown(&mut self) {
        let ids: Vec<ConnId> = self.conns.keys().copied().collect();
        for id in ids {
            self.guarded(|net| net.close_conn(id, "server stopping"));
        }
        log_line!(self.log, INFO, "stopped");
    }

    fn command(&mut self, command: Command) {
        let now = Instant::now();
        match command {
            Command::CreateCall { ufrags, reply } => drop(reply.send(self.create_call(ufrags, now))),
            Command::GetCall { id, reply } => drop(reply.send(self.get_call(id, now))),
            Command::DeleteCall { id, reply } => drop(reply.send(self.delete_call(id))),
            Command::CreateRoom { reply } => drop(reply.send(self.create_room(now))),
            Command::GetRoom { id, reply } => drop(reply.send(self.get_room(id))),
            Command::DeleteRoom { id, reply } => drop(reply.send(self.delete_room(id))),
            Command::Publish { room, user, offer, reply } => {
                let answer = self.publish(room, user, *offer, now);
                drop(reply.send(answer));
            }
            Command::Subscribe { room, user, other, offer, reply } => {
                let answer = self.subscribe(room, user, other, *offer, now);
                drop(reply.send(answer));
            }
            Command::DeleteParticipant { room, user, reply } => drop(reply.send(self.delete_participant(room, &user))),
            Command::Stop => {}
        }
        self.settle(now);
    }

    fn timers_due(&mut self, now: Instant) {
        let due: Vec<ConnId> = self.timers.iter().take_while(|(at, _)| *at <= now).map(|(_, id)| *id).collect();
        for id in due {
            let Some(conn) = self.conns.get_mut(&id) else { continue };
            self.touching = Some(id);
            let _ = conn.rtc.handle_input(Input::Timeout(now));
            self.drain(id);
        }
        self.settle(now);
    }

    fn housekeeping(&mut self, now: Instant) {
        self.room_housekeeping(now);
        self.call_housekeeping(now);
        self.settle(now);
    }

    fn datagram(&mut self, from: SocketAddr, data: &[u8], now: Instant) {
        let username = binding_username(data);
        match self.owners.get(&from).copied() {
            Some(Owner::Call(call, side)) => match username {
                Some(name) if self.by_username.get(name) != Some(&(call, side)) => {}
                _ => return self.relay(call, side, data, now),
            },
            Some(Owner::Conn(id)) => match username {
                Some(name) if self.conns.get(&id).is_some_and(|c| !name.starts_with(&format!("{}:", c.ufrag))) => {}
                _ => return self.receive(id, from, data, now),
            },
            None => {}
        }
        // (c): not a binding request, and nobody's address.
        let Some(name) = username else { return };
        // (b): a room connection by its ufrag, confirmed by str0m.
        let ours = name.split_once(':').map_or(name, |(receiver, _)| receiver);
        if let Some(&id) = self.by_ufrag.get(ours) {
            let accepts = match (self.conns.get(&id), data.try_into()) {
                (Some(conn), Ok(contents)) => conn.rtc.accepts(&self.input(from, contents, now)),
                _ => false,
            };
            if accepts {
                self.latch(from, Owner::Conn(id));
                return self.receive(id, from, data, now);
            }
        }
        // (b): a call reserved for this USERNAME pair.
        if let Some(&(call, side)) = self.by_username.get(name) {
            self.latch_call(call, side, from);
            self.relay(call, side, data, now);
        }
    }

    pub(crate) fn input<'a>(&self, from: SocketAddr, contents: str0m::net::DatagramRecv<'a>, now: Instant) -> Input<'a> {
        Input::Receive(now, Receive { proto: Protocol::Udp, source: from, destination: self.public, contents })
    }

    /// A datagram for a room connection: one mutation, then the poll.
    fn receive(&mut self, id: ConnId, from: SocketAddr, data: &[u8], now: Instant) {
        let Ok(contents) = data.try_into() else { return };
        let input = self.input(from, contents, now);
        let Some(conn) = self.conns.get_mut(&id) else { return };
        conn.last_packet = now;
        conn.bytes_in += data.len() as u64;
        self.touching = Some(id);
        // A datagram str0m has no use for is not an error of the connection; a dead `Rtc` is
        // seen by the poll.
        let _ = conn.rtc.handle_input(input);
        self.drain(id);
        self.settle(now);
    }

    /// `from` is now `owner`'s; whoever had it before loses it.
    pub(crate) fn latch(&mut self, from: SocketAddr, owner: Owner) {
        if let Some(before) = self.owners.insert(from, owner) {
            if before == owner {
                return;
            }
            self.unlatch_from(before, from);
        }
        if let Owner::Conn(id) = owner {
            if let Some(conn) = self.conns.get_mut(&id) {
                conn.addrs.push(from);
                if conn.addrs.len() > MAX_ADDRS {
                    let oldest = conn.addrs.remove(0);
                    if self.owners.get(&oldest) == Some(&owner) {
                        self.owners.remove(&oldest);
                    }
                }
            }
        }
    }

    /// Takes `addr` off `owner`'s own record (the address map is the caller's).
    fn unlatch_from(&mut self, owner: Owner, addr: SocketAddr) {
        match owner {
            Owner::Conn(id) => {
                if let Some(conn) = self.conns.get_mut(&id) {
                    conn.addrs.retain(|a| *a != addr);
                }
            }
            Owner::Call(call, side) => {
                if let Some(call) = self.calls.get_mut(&call) {
                    if call.sides[side].addr == Some(addr) {
                        call.sides[side].addr = None;
                    }
                }
            }
        }
    }

    /// Polls one connection dry: sends what it has to send, turns its events into work, files
    /// its next timeout.
    pub(crate) fn drain(&mut self, id: ConnId) {
        self.touching = Some(id);
        let Net { conns, socket, work, timers, .. } = self;
        let Some(conn) = conns.get_mut(&id) else { return };
        let mut errors = 0;
        let at = loop {
            match conn.rtc.poll_output() {
                Ok(Output::Timeout(at)) => break Some(at),
                Ok(Output::Transmit(t)) => {
                    conn.bytes_out += t.contents.len() as u64;
                    let _ = socket.send_to(&t.contents, t.destination);
                }
                Ok(Output::Event(event)) => crate::room::event(id, conn, event, work),
                // One packet's error is that packet's (str0m dropped it and goes on); only an
                // `Rtc` that says it is finished is finished.
                Err(e) => {
                    errors += 1;
                    if !conn.rtc.is_alive() || errors > 64 {
                        conn.failure.get_or_insert_with(|| e.to_string());
                        break None;
                    }
                }
            }
        };
        if let Some(at) = at {
            if conn.timer != Some(at) {
                if let Some(before) = conn.timer {
                    timers.remove(&(before, id));
                }
                timers.insert((at, id));
                conn.timer = Some(at);
            }
        }
        if (!conn.rtc.is_alive() || at.is_none()) && !conn.closing {
            conn.closing = true;
            work.push_back(Work::Dead(id));
        }
    }

    /// Applies the queued work until there is none: each item one mutation and its poll.
    pub(crate) fn settle(&mut self, now: Instant) {
        while let Some(work) = self.work.pop_front() {
            self.apply(work, now);
        }
    }

    /// Forgets a connection without a goodbye (its `Rtc` may be broken: it panicked).
    fn discard(&mut self, id: ConnId) {
        if let Some(conn) = self.conns.get_mut(&id) {
            conn.rtc.disconnect();
        }
        self.close_conn(id, "internal error");
    }
}

/// The USERNAME of a STUN binding request; None for anything else (another STUN message, RTP,
/// RTCP, DTLS, a malformed one).
fn binding_username(data: &[u8]) -> Option<&str> {
    if !stun::is_stun(data) {
        return None;
    }
    let message = stun::Message::parse(data).ok()?;
    if message.class() != stun::Class::Request || message.method() != stun::BINDING {
        return None;
    }
    std::str::from_utf8(message.get(stun::attr::USERNAME)?).ok()
}

/// A fresh id: SipHash keyed by std's `RandomState` (OS randomness) over a counter, as core's
/// STUN transaction ids. Unguessable enough for a private API's handles; not a vetted CSPRNG.
pub(crate) fn random_id() -> Id {
    use std::hash::BuildHasher;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::hash::RandomState::new().hash_one((n, Instant::now()))
}

pub(crate) fn show_id(id: Id) -> String {
    format!("{id:016x}")
}

pub(crate) fn parse_id(text: &str) -> Option<Id> {
    if text.len() != 16 {
        return None;
    }
    u64::from_str_radix(text, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tinyrtc::stun::{attr, Class, MessageWriter, TransactionId, BINDING};

    fn request(username: Option<&str>, class: Class) -> Vec<u8> {
        let mut w = MessageWriter::new(BINDING, class, TransactionId([7; 12]));
        if let Some(name) = username {
            w.attribute(attr::USERNAME, name.as_bytes());
        }
        w.attribute(attr::MESSAGE_INTEGRITY, &[0; 20]).fingerprint();
        w.into_bytes()
    }

    #[test]
    fn only_a_binding_request_has_a_username() {
        assert_eq!(binding_username(&request(Some("abcd:efgh"), Class::Request)), Some("abcd:efgh"));
        assert_eq!(binding_username(&request(None, Class::Request)), None);
        assert_eq!(binding_username(&request(Some("abcd:efgh"), Class::Success)), None);
        assert_eq!(binding_username(&request(Some("abcd:efgh"), Class::Indication)), None);
        let mut broken = request(Some("abcd:efgh"), Class::Request);
        let last = broken.len() - 1;
        broken[last] ^= 1; // the FINGERPRINT no longer matches
        assert_eq!(binding_username(&broken), None);
        assert_eq!(binding_username(&[0x80, 0x60, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]), None);
        assert_eq!(binding_username(&[]), None);
    }

    #[test]
    fn ids_show_and_parse_back() {
        let id = random_id();
        assert_ne!(id, random_id());
        assert_eq!(parse_id(&show_id(id)), Some(id));
        assert_eq!(parse_id(&show_id(5)), Some(5));
        assert_eq!(parse_id("5"), None);
        assert_eq!(parse_id("zzzzzzzzzzzzzzzz"), None);
    }
}
