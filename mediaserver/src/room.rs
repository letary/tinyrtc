//! ROOMS: the server is a peer. A participant PUBLISHES on one connection (phone -> server) and
//! SUBSCRIBES to each other participant on one connection per publisher (server -> phone). Each
//! connection is a str0m `Rtc` of core's profile, ICE-lite, answering the phone's offer with the
//! public address as its only candidate — one HTTP request and its response set it up.
//!
//! Media is forwarded frame by frame (str0m's frame level, as its chat.rs): a frame of a publish
//! connection is written into every subscribe connection of that publisher, its payload type
//! matched by codec parameters. Each hop repairs its own losses (NACK / RTX); a video frame the
//! publish hop could not repair is not passed on, and its subscribers wait for the next keyframe.
//!
//! Keyframes: a subscriber's request, a new subscriber, a loss at the ingress all ask the
//! publisher, at most once per `keyframe_interval`; a request that falls inside the interval
//! (or before the publisher is connected) is kept and sent when it may be.
//!
//! A participant exists while it has a connection; a room is empty when it has no participant.
//! A publish connection that closes closes every subscribe connection of it (the phones see it
//! close: how they learn a participant left when the backend is late) — unless the participant
//! published again, in which case its subscribers stay and are fed from the new connection.

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::time::Instant;

use str0m::bwe::Bitrate;
use str0m::change::SdpOffer;
use str0m::format::Codec;
use str0m::media::{KeyframeRequestKind, MediaData, MediaKind, MediaTime, Mid};
use str0m::{Candidate, Event, IceConnectionState, Rtc};

use crate::http::{json, ParticipantInfo, RoomCreated, RoomInfo};
use crate::net::{random_id, show_id, ApiError, Body, ConnId, Id, Net, Owner, Response};

const AUDIO: usize = 0;
const VIDEO: usize = 1;

#[derive(Default)]
pub(crate) struct Room {
    pub(crate) participants: BTreeMap<String, Participant>,
    /// Since when it has no participant.
    pub(crate) empty_since: Option<Instant>,
}

#[derive(Default)]
pub(crate) struct Participant {
    pub(crate) publish: Option<ConnId>,
    /// Publisher -> our subscribe connection of it.
    pub(crate) subscriptions: BTreeMap<String, ConnId>,
    /// Subscriber -> its subscribe connection of us.
    pub(crate) subscribers: BTreeMap<String, ConnId>,
    /// The bytes of its connections already closed.
    pub(crate) closed_in: u64,
    pub(crate) closed_out: u64,
}

impl Participant {
    fn is_empty(&self) -> bool {
        self.publish.is_none() && self.subscriptions.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Role {
    Publish,
    Subscribe(String),
}

pub(crate) struct Conn {
    pub(crate) rtc: Rtc,
    pub(crate) room: Id,
    pub(crate) user: String,
    pub(crate) role: Role,
    /// Our ICE ufrag on this connection.
    pub(crate) ufrag: String,
    pub(crate) addrs: Vec<SocketAddr>,
    pub(crate) last_packet: Instant,
    pub(crate) connected: bool,
    pub(crate) ice: IceConnectionState,
    /// [audio, video]
    pub(crate) mids: [Option<Mid>; 2],
    pub(crate) bytes_in: u64,
    pub(crate) bytes_out: u64,
    pub(crate) timer: Option<Instant>,
    /// A dead `Work` is queued: it is on its way out.
    pub(crate) closing: bool,
    pub(crate) remote_closed: bool,
    pub(crate) failure: Option<String>,
    /// Publish: the last keyframe request sent to the phone.
    pub(crate) keyframe_at: Option<Instant>,
    /// Subscribe: no video until a keyframe (a new subscriber, a new source, a loss upstream).
    pub(crate) wait_keyframe: bool,
    /// Subscribe: the outgoing media time of [audio, video].
    pub(crate) rebase: [Rebase; 2],
}

/// The work a poll brings out for other connections than the polled one.
pub(crate) enum Work {
    Media(ConnId, Box<MediaData>),
    /// A subscriber asked for a keyframe.
    KeyframeAsked(ConnId),
    Connected(ConnId),
    /// The `Rtc` is finished (closed by the phone, or failed).
    Dead(ConnId),
}

/// One event of one connection's poll (the poll owns the connection; anything for another one
/// is queued).
pub(crate) fn event(id: ConnId, conn: &mut Conn, event: Event, work: &mut VecDeque<Work>) {
    match event {
        Event::Connected => {
            conn.connected = true;
            work.push_back(Work::Connected(id));
        }
        Event::IceConnectionStateChange(state) => conn.ice = state,
        Event::MediaAdded(added) => {
            let slot = match added.kind {
                MediaKind::Audio => AUDIO,
                MediaKind::Video => VIDEO,
            };
            conn.mids[slot] = conn.mids[slot].or(Some(added.mid));
        }
        Event::MediaData(data) if conn.role == Role::Publish => work.push_back(Work::Media(id, Box::new(data))),
        Event::KeyframeRequest(_) if conn.role != Role::Publish => work.push_back(Work::KeyframeAsked(id)),
        Event::Closed => conn.remote_closed = true,
        // Data channels are not forwarded in rooms (this version); the stats are not read.
        _ => {}
    }
}

/// The media time a subscribe connection sends with. The first source's time passes unchanged;
/// when the source changes (the participant published again: a new clock, starting anywhere),
/// the new one is shifted to continue where the last left off, advanced by the wall time
/// between — so the subscriber's stream never jumps back.
#[derive(Default)]
pub(crate) struct Rebase {
    source: Option<ConnId>,
    offset: u64,
    last: Option<(u64, Instant)>,
}

impl Rebase {
    fn map(&mut self, source: ConnId, time: MediaTime, now: Instant) -> MediaTime {
        let t = time.numer();
        if self.source != Some(source) {
            let start = match self.last {
                Some((last, at)) => {
                    let ticks = now.saturating_duration_since(at).as_secs_f64() * f64::from(time.denom());
                    last.wrapping_add((ticks as u64).max(1))
                }
                None => t,
            };
            self.offset = start.wrapping_sub(t);
            self.source = Some(source);
        }
        let out = t.wrapping_add(self.offset);
        self.last = Some((out, now));
        MediaTime::new(out, time.frequency())
    }
}

/// A participant's id, as it appears in a path: 1..=64 of [A-Za-z0-9._~-].
pub(crate) fn valid_user(user: &str) -> bool {
    (1..=64).contains(&user.len()) && user.bytes().all(|b| b.is_ascii_alphanumeric() || b"._~-".contains(&b))
}

impl Net {
    pub(crate) fn create_room(&mut self, now: Instant) -> Response {
        let id = loop {
            let id = random_id();
            if !self.rooms.contains_key(&id) {
                break id;
            }
        };
        self.rooms.insert(id, Room { participants: BTreeMap::new(), empty_since: Some(now) });
        log_line!(self.log, INFO, "room {}: created", show_id(id));
        json(201, &RoomCreated { id: show_id(id) })
    }

    pub(crate) fn delete_room(&mut self, id: Id) -> Response {
        let Some(room) = self.rooms.get(&id) else { return Err(ApiError::new(404, "no such room")) };
        let conns: Vec<ConnId> = room
            .participants
            .values()
            .flat_map(|p| p.publish.iter().chain(p.subscriptions.values()).copied().collect::<Vec<_>>())
            .collect();
        for conn in conns {
            self.close_conn(conn, "room deleted");
        }
        self.rooms.remove(&id);
        log_line!(self.log, INFO, "room {}: deleted", show_id(id));
        Ok((204, Body::Empty))
    }

    pub(crate) fn get_room(&self, id: Id) -> Response {
        let Some(room) = self.rooms.get(&id) else { return Err(ApiError::new(404, "no such room")) };
        let participants = room
            .participants
            .iter()
            .map(|(user, p)| {
                let conns: Vec<&Conn> =
                    p.publish.iter().chain(p.subscriptions.values()).filter_map(|id| self.conns.get(id)).collect();
                ParticipantInfo {
                    id: user,
                    publishing: p.publish.is_some(),
                    connected: conns.iter().any(|c| c.connected),
                    subscribers: p.subscribers.keys().map(String::as_str).collect(),
                    bytes_in: p.closed_in + conns.iter().map(|c| c.bytes_in).sum::<u64>(),
                    bytes_out: p.closed_out + conns.iter().map(|c| c.bytes_out).sum::<u64>(),
                }
            })
            .collect();
        json(200, &RoomInfo { participants })
    }

    /// A connection of the profile, ICE-lite, answering `offer`. Returns its id and the answer.
    fn answer(&mut self, room: Id, user: &str, role: Role, offer: SdpOffer, now: Instant) -> Result<(ConnId, String), ApiError> {
        let config = tinyrtc::profile::config()
            .set_ice_lite(true)
            .set_dtls_cert(self.cert.clone())
            // Nothing here reads str0m's stats: they would cost every connection a second's work.
            .set_stats_interval(None);
        let mut rtc = config.build(now);
        // Only read: made at build, the first half of every USERNAME the phone sends us.
        let ufrag = rtc.direct_api().local_ice_credentials().ufrag;
        let id = self.next_conn;
        self.next_conn += 1;
        self.conns.insert(
            id,
            Conn {
                rtc,
                room,
                user: user.to_owned(),
                role: role.clone(),
                ufrag: ufrag.clone(),
                addrs: Vec::new(),
                last_packet: now,
                connected: false,
                ice: IceConnectionState::New,
                mids: [None; 2],
                bytes_in: 0,
                bytes_out: 0,
                timer: None,
                closing: false,
                remote_closed: false,
                failure: None,
                keyframe_at: None,
                wait_keyframe: true,
                rebase: Default::default(),
            },
        );
        // One mutation, then the poll; three times.
        let public = self.public;
        let subscriber_bitrate = self.subscriber_bitrate;
        let conn = self.conns.get_mut(&id).expect("just inserted");
        self.touching = Some(id);
        if let Ok(candidate) = Candidate::host(public, "udp") {
            conn.rtc.add_local_candidate(candidate);
        }
        self.drain(id);
        let conn = self.conns.get_mut(&id).expect("just inserted");
        let answer = match conn.rtc.sdp_api().accept_offer(offer) {
            Ok(answer) => answer.to_sdp_string(),
            Err(e) => {
                self.conns.remove(&id);
                self.forget_timer(id);
                return Err(ApiError::new(400, format!("the offer cannot be answered: {e}")));
            }
        };
        self.drain(id);
        if let Role::Subscribe(_) = role {
            // The estimate probes up to what a publisher may send, so the pacer lets it through.
            let conn = self.conns.get_mut(&id).expect("just inserted");
            conn.rtc.bwe().set_bitrate(Bitrate::ZERO, Bitrate::bps(subscriber_bitrate));
            self.drain(id);
        }
        // The poll may already have found it dead; then it is not anyone's yet.
        self.work.retain(|w| !matches!(w, Work::Dead(dead) if *dead == id));
        if self.conns.get(&id).is_some_and(|c| !c.rtc.is_alive()) {
            self.conns.remove(&id);
            self.forget_timer(id);
            return Err(ApiError::new(400, "the offer cannot be answered"));
        }
        self.by_ufrag.insert(ufrag, id);
        Ok((id, answer))
    }

    fn forget_timer(&mut self, id: ConnId) {
        self.timers.retain(|(_, conn)| *conn != id);
    }

    pub(crate) fn publish(&mut self, room: Id, user: String, offer: SdpOffer, now: Instant) -> Response {
        if !self.rooms.contains_key(&room) {
            return Err(ApiError::new(404, "no such room"));
        }
        let (id, answer) = self.answer(room, &user, Role::Publish, offer, now)?;
        let room_entry = self.rooms.get_mut(&room).expect("checked");
        room_entry.empty_since = None;
        let participant = room_entry.participants.entry(user.clone()).or_default();
        let before = participant.publish.replace(id);
        let subscribers: Vec<ConnId> = participant.subscribers.values().copied().collect();
        log_line!(
            self.log,
            INFO,
            "room {}: {user} publishes (connection {id}){}",
            show_id(room),
            if before.is_some() { ", replacing the one before" } else { "" }
        );
        if let Some(before) = before {
            // Its subscribers stay: the new connection feeds them from its first keyframe.
            self.close_conn(before, "replaced by a new publish");
            for sub in subscribers {
                if let Some(conn) = self.conns.get_mut(&sub) {
                    conn.wait_keyframe = true;
                }
            }
            self.keyframes.insert(id);
        }
        Ok((200, Body::Sdp(answer)))
    }

    pub(crate) fn subscribe(&mut self, room: Id, user: String, other: String, offer: SdpOffer, now: Instant) -> Response {
        let Some(room_entry) = self.rooms.get(&room) else { return Err(ApiError::new(404, "no such room")) };
        if user == other {
            return Err(ApiError::new(400, "a participant does not subscribe to itself"));
        }
        if room_entry.participants.get(&other).is_none_or(|p| p.publish.is_none()) {
            return Err(ApiError::new(404, format!("{other} is not publishing")));
        }
        let (id, answer) = self.answer(room, &user, Role::Subscribe(other.clone()), offer, now)?;
        let room_entry = self.rooms.get_mut(&room).expect("checked");
        room_entry.empty_since = None;
        room_entry.participants.entry(other.clone()).or_default().subscribers.insert(user.clone(), id);
        let before = room_entry.participants.entry(user.clone()).or_default().subscriptions.insert(other.clone(), id);
        log_line!(self.log, INFO, "room {}: {user} subscribes to {other} (connection {id})", show_id(room));
        if let Some(before) = before {
            self.close_conn(before, "replaced by a new subscribe");
        }
        Ok((200, Body::Sdp(answer)))
    }

    pub(crate) fn delete_participant(&mut self, room: Id, user: &str) -> Response {
        let Some(room_entry) = self.rooms.get(&room) else { return Err(ApiError::new(404, "no such room")) };
        let Some(p) = room_entry.participants.get(user) else { return Err(ApiError::new(404, "no such participant")) };
        let conns: Vec<ConnId> = p.publish.iter().chain(p.subscriptions.values()).copied().collect();
        for conn in conns {
            self.close_conn(conn, "participant deleted");
        }
        // Closing its publish connection closed everyone's subscriptions of it, and the last of
        // its connections took the participant with it.
        log_line!(self.log, INFO, "room {}: {user} deleted", show_id(room));
        Ok((204, Body::Empty))
    }

    /// Closes a connection (a goodbye to the phone if it is alive) and everything that hangs on
    /// it: a publish connection takes its subscribe connections along.
    pub(crate) fn close_conn(&mut self, id: ConnId, reason: &str) {
        let Some(mut conn) = self.conns.remove(&id) else { return };
        if conn.rtc.is_alive() {
            self.touching = Some(id);
            // RTCP BYE, the SCTP shutdown, DTLS close_notify: the phone sees Closed at once.
            if conn.rtc.close().is_ok() {
                while let Ok(output) = conn.rtc.poll_output() {
                    match output {
                        str0m::Output::Transmit(t) => {
                            conn.bytes_out += t.contents.len() as u64;
                            let _ = self.socket.send_to(&t.contents, t.destination);
                        }
                        str0m::Output::Timeout(_) => break,
                        str0m::Output::Event(_) => {}
                    }
                }
            }
        }
        if let Some(at) = conn.timer {
            self.timers.remove(&(at, id));
        }
        if self.by_ufrag.get(&conn.ufrag) == Some(&id) {
            self.by_ufrag.remove(&conn.ufrag);
        }
        for addr in &conn.addrs {
            if self.owners.get(addr) == Some(&Owner::Conn(id)) {
                self.owners.remove(addr);
            }
        }
        self.keyframes.remove(&id);
        let what = match &conn.role {
            Role::Publish => "publish".to_owned(),
            Role::Subscribe(other) => format!("subscription to {other}"),
        };
        log_line!(self.log, INFO, "room {}: {}'s {what} (connection {id}) closed: {reason}", show_id(conn.room), conn.user);

        let Some(room) = self.rooms.get_mut(&conn.room) else { return };
        let mut followers = Vec::new();
        if let Some(p) = room.participants.get_mut(&conn.user) {
            p.closed_in += conn.bytes_in;
            p.closed_out += conn.bytes_out;
            match &conn.role {
                Role::Publish => {
                    if p.publish == Some(id) {
                        p.publish = None;
                        followers = p.subscribers.values().copied().collect();
                    }
                }
                Role::Subscribe(other) => {
                    if p.subscriptions.get(other) == Some(&id) {
                        p.subscriptions.remove(other);
                    }
                }
            }
        }
        if let Role::Subscribe(other) = &conn.role {
            if let Some(publisher) = room.participants.get_mut(other) {
                if publisher.subscribers.get(&conn.user) == Some(&id) {
                    publisher.subscribers.remove(&conn.user);
                }
            }
        }
        for follower in followers {
            self.close_conn(follower, &format!("{} left", conn.user));
        }
        self.prune(conn.room, &conn.user);
        if let Role::Subscribe(other) = &conn.role {
            self.prune(conn.room, other);
        }
    }

    /// A participant with no connection left is gone; a room with none is empty from now.
    fn prune(&mut self, room: Id, user: &str) {
        let Some(room_entry) = self.rooms.get_mut(&room) else { return };
        if room_entry.participants.get(user).is_some_and(|p| p.is_empty() && p.subscribers.is_empty()) {
            room_entry.participants.remove(user);
            log_line!(self.log, INFO, "room {}: {user} left", show_id(room));
        }
        if room_entry.participants.is_empty() && room_entry.empty_since.is_none() {
            room_entry.empty_since = Some(std::time::Instant::now());
        }
    }

    pub(crate) fn apply(&mut self, work: Work, now: Instant) {
        match work {
            Work::Media(from, data) => self.forward(from, *data, now),
            Work::KeyframeAsked(sub) => {
                if let Some(publisher) = self.publisher_of(sub) {
                    self.want_keyframe(publisher, now);
                }
            }
            Work::Connected(id) => {
                let Some(conn) = self.conns.get(&id) else { return };
                let what = match &conn.role {
                    Role::Publish => "publish".to_owned(),
                    Role::Subscribe(other) => format!("subscription to {other}"),
                };
                log_line!(self.log, INFO, "room {}: {}'s {what} (connection {id}) connected", show_id(conn.room), conn.user);
                if conn.role == Role::Publish {
                    if self.keyframes.contains(&id) {
                        self.want_keyframe(id, now);
                    }
                } else if let Some(publisher) = self.publisher_of(id) {
                    // A new subscriber needs a picture to start from.
                    self.want_keyframe(publisher, now);
                }
            }
            Work::Dead(id) => {
                let reason = match self.conns.get(&id) {
                    Some(conn) if conn.remote_closed => "closed by the phone".to_owned(),
                    Some(conn) => format!("failed: {}", conn.failure.as_deref().unwrap_or("the connection ended")),
                    None => return,
                };
                self.close_conn(id, &reason);
            }
        }
    }

    /// The current publish connection of the participant a subscribe connection receives.
    fn publisher_of(&self, sub: ConnId) -> Option<ConnId> {
        let conn = self.conns.get(&sub)?;
        let Role::Subscribe(other) = &conn.role else { return None };
        self.rooms.get(&conn.room)?.participants.get(other)?.publish
    }

    /// Asks the publisher's phone for a keyframe now, or as soon as it may be asked.
    fn want_keyframe(&mut self, publisher: ConnId, now: Instant) {
        let interval = self.keyframe_interval;
        let Some(conn) = self.conns.get_mut(&publisher) else { return };
        let too_soon = conn.keyframe_at.is_some_and(|at| now < at + interval);
        let Some(mid) = conn.mids[VIDEO].filter(|_| conn.connected && !too_soon) else {
            self.keyframes.insert(publisher);
            return;
        };
        self.touching = Some(publisher);
        // A phone that has sent no video yet has no stream to ask: kept until it has.
        let asked = conn.rtc.writer(mid).is_some_and(|mut w| w.request_keyframe(None, KeyframeRequestKind::Pli).is_ok());
        if asked {
            conn.keyframe_at = Some(now);
            self.keyframes.remove(&publisher);
        } else {
            self.keyframes.insert(publisher);
        }
        self.drain(publisher);
    }

    /// One frame of a publish connection into each subscribe connection of its participant.
    fn forward(&mut self, from: ConnId, data: MediaData, now: Instant) {
        let Some(conn) = self.conns.get(&from) else { return };
        let Some(p) = self.rooms.get(&conn.room).and_then(|r| r.participants.get(&conn.user)) else { return };
        // A replaced connection may still say something on its way out: not heard.
        if p.publish != Some(from) {
            return;
        }
        let slot = match data.params.spec().codec {
            Codec::Opus => AUDIO,
            Codec::H264 => VIDEO,
            _ => return,
        };
        let subscribers: Vec<ConnId> = p.subscribers.values().copied().collect();
        let keyframe = slot == VIDEO && data.is_keyframe();
        if slot == VIDEO && !data.contiguous && !keyframe {
            // Lost upstream for good: what follows decodes against a picture nobody has.
            for sub in &subscribers {
                if let Some(conn) = self.conns.get_mut(sub) {
                    conn.wait_keyframe = true;
                }
            }
            self.want_keyframe(from, now);
        }
        for sub in subscribers {
            self.write(sub, from, slot, keyframe, &data, now);
        }
    }

    fn write(&mut self, sub: ConnId, from: ConnId, slot: usize, keyframe: bool, data: &MediaData, now: Instant) {
        let Some(conn) = self.conns.get_mut(&sub) else { return };
        if !conn.connected || conn.closing {
            return;
        }
        if slot == VIDEO && conn.wait_keyframe {
            if !keyframe {
                return;
            }
            conn.wait_keyframe = false;
        }
        let Some(mid) = conn.mids[slot] else { return };
        let time = conn.rebase[slot].map(from, data.time, now);
        self.touching = Some(sub);
        let Some(writer) = conn.rtc.writer(mid) else { return };
        let Some(pt) = writer.match_params(data.params) else { return };
        let mut writer = writer;
        if let Some(orientation) = data.ext_vals.video_orientation {
            writer = writer.video_orientation(orientation);
        }
        if let (Some(level), Some(voice)) = (data.ext_vals.audio_level, data.ext_vals.voice_activity) {
            writer = writer.audio_level(level, voice);
        }
        let _ = writer.write(pt, data.network_time, time, data.data.clone());
        self.drain(sub);
    }

    pub(crate) fn room_housekeeping(&mut self, now: Instant) {
        let idle: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(_, c)| now.saturating_duration_since(c.last_packet) >= self.connection_idle)
            .map(|(id, _)| *id)
            .collect();
        for id in idle {
            let secs = self.connection_idle.as_secs_f64();
            self.close_conn(id, &format!("idle (no packet for {secs} s)"));
        }
        let owed: Vec<ConnId> = self.keyframes.iter().copied().collect();
        for id in owed {
            self.want_keyframe(id, now);
        }
        let empty_for = self.room_empty;
        let expired: Vec<Id> = self
            .rooms
            .iter()
            .filter(|(_, r)| r.empty_since.is_some_and(|since| now.saturating_duration_since(since) >= empty_for))
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.rooms.remove(&id);
            log_line!(self.log, INFO, "room {}: deleted (empty for {} s)", show_id(id), empty_for.as_secs_f64());
        }
    }
}
