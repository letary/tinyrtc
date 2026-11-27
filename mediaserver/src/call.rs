//! CALLS: the blind relay of a 1:1 call. The server never has a key and reads one thing only.
//!
//! The two phones exchange offer and answer through the backend; the backend reserves the call
//! here with the two ICE ufrags it read from the two descriptions, and gives each phone the
//! server's public address as a candidate of the other side: a `typ relay` one (`relay_candidate`).
//! A phone that is `relay_only` takes no other kind, so for it this is the only path (its own
//! addresses are never in a description, and it never checks the other side's); a phone that is
//! not takes it as its last choice, after any direct path. The phones' ICE checks then arrive here:
//!
//!   a STUN binding request from an address nobody owns is parsed only as far as its USERNAME,
//!   "<receiver ufrag>:<sender ufrag>". If the pair is a reserved call's, the sender's address is
//!   latched to the sender's side. From then on every datagram from a latched address — ICE,
//!   DTLS, SRTP, SCTP, whatever it is — is forwarded unchanged to the other side's latched
//!   address (dropped while the other side has none).
//!
//!   A later binding request with the same pair from a NEW address re-latches that side (a phone
//!   that moved networks: its checks come from the new address first).
//!
//! Security, and its limit: only someone who knows BOTH ufrags of a reserved call can latch; one
//! ufrag, or none, latches nothing and gets nothing forwarded. MESSAGE-INTEGRITY is not verified
//! (the server has no ICE password), so someone who saw both ufrags (they travel in the
//! descriptions, through the backend) can take a side's place. The media stays end-to-end
//! encrypted either way: what such a someone can do is cut the call, not listen to it.

use std::net::SocketAddr;
use std::time::Instant;

use crate::http::{json, CallCreated, CallInfo, SideInfo};
use crate::net::{random_id, show_id, ApiError, Body, Id, Net, Owner, Response};

#[derive(Debug, Default)]
pub(crate) struct Side {
    pub(crate) ufrag: String,
    pub(crate) addr: Option<SocketAddr>,
    /// Datagrams (and their bytes) received from this side.
    pub(crate) packets: u64,
    pub(crate) bytes: u64,
}

#[derive(Debug)]
pub(crate) struct Call {
    pub(crate) sides: [Side; 2],
    pub(crate) last: Instant,
}

/// The `candidate:` line both phones of a call are given: the server's public address, typed
/// `relay` — what it is to both, and the one kind a relay-only connection (core's, the web half's)
/// takes. The priority is the lowest the RFC 8445 §5.1.2 formula gives a component-1 candidate
/// (type preference 0, local preference 0: 255), so every direct pair outranks it and a call that
/// can go direct still does. `raddr` / `rport` say nothing (0.0.0.0 / 0, as browsers write them to
/// hide an address); they are there because a parser may want them on a non-host candidate (str0m's
/// candidate.rs names Firefox's). str0m and Chrome take the line with them or without.
pub(crate) fn relay_candidate(public: SocketAddr) -> String {
    let unspecified = if public.is_ipv4() { "0.0.0.0" } else { "::" };
    format!("candidate:1 1 udp 255 {} {} typ relay raddr {unspecified} rport 0", public.ip(), public.port())
}

/// RFC 8445 §5.3: 4 to 256 ice-chars (ALPHA / DIGIT / "+" / "/").
pub(crate) fn valid_ufrag(ufrag: &str) -> bool {
    (4..=256).contains(&ufrag.len()) && ufrag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

impl Net {
    pub(crate) fn create_call(&mut self, ufrags: [String; 2], now: Instant) -> Response {
        let [a, b] = ufrags;
        // The receiver's ufrag first: side 1 sends "a:b", side 0 sends "b:a".
        let keys = [format!("{b}:{a}"), format!("{a}:{b}")];
        if keys.iter().any(|key| self.by_username.contains_key(key)) {
            return Err(ApiError::new(409, "a call with these ufrags is already reserved"));
        }
        let id = loop {
            let id = random_id();
            if !self.calls.contains_key(&id) {
                break id;
            }
        };
        for (side, key) in keys.into_iter().enumerate() {
            self.by_username.insert(key, (id, side));
        }
        log_line!(self.log, INFO, "call {}: reserved for {a} / {b}", show_id(id));
        let sides = [Side { ufrag: a, ..Side::default() }, Side { ufrag: b, ..Side::default() }];
        self.calls.insert(id, Call { sides, last: now });
        json(201, &CallCreated { id: show_id(id), candidate: &self.call_candidate })
    }

    pub(crate) fn get_call(&self, id: Id, now: Instant) -> Response {
        let Some(call) = self.calls.get(&id) else { return Err(ApiError::new(404, "no such call")) };
        let sides = call.sides.each_ref().map(|s| SideInfo {
            ufrag: &s.ufrag,
            connected: s.addr.is_some(),
            packets: s.packets,
            bytes: s.bytes,
        });
        let idle_ms = u64::try_from(now.saturating_duration_since(call.last).as_millis()).unwrap_or(u64::MAX);
        json(200, &CallInfo { sides, idle_ms })
    }

    pub(crate) fn delete_call(&mut self, id: Id) -> Response {
        if self.release(id, "deleted") {
            Ok((204, Body::Empty))
        } else {
            Err(ApiError::new(404, "no such call"))
        }
    }

    fn release(&mut self, id: Id, reason: &str) -> bool {
        let Some(call) = self.calls.remove(&id) else { return false };
        let [a, b] = [&call.sides[0].ufrag, &call.sides[1].ufrag];
        self.by_username.remove(&format!("{b}:{a}"));
        self.by_username.remove(&format!("{a}:{b}"));
        for (side, s) in call.sides.iter().enumerate() {
            if let Some(addr) = s.addr {
                if self.owners.get(&addr) == Some(&Owner::Call(id, side)) {
                    self.owners.remove(&addr);
                }
            }
        }
        log_line!(
            self.log,
            INFO,
            "call {}: released: {reason} ({} + {} datagrams relayed)",
            show_id(id),
            call.sides[0].packets,
            call.sides[1].packets
        );
        true
    }

    /// `from` sent a binding request with this call's pair as `side`: it is that side now.
    pub(crate) fn latch_call(&mut self, id: Id, side: usize, from: SocketAddr) {
        let Some(call) = self.calls.get_mut(&id) else { return };
        let before = call.sides[side].addr;
        if before == Some(from) {
            return;
        }
        if let Some(old) = before {
            if self.owners.get(&old) == Some(&Owner::Call(id, side)) {
                self.owners.remove(&old);
            }
        }
        self.latch(from, Owner::Call(id, side));
        if let Some(call) = self.calls.get_mut(&id) {
            call.sides[side].addr = Some(from);
        }
        match before {
            None => log_line!(self.log, INFO, "call {}: side {side} latched", show_id(id)),
            Some(_) => log_line!(self.log, INFO, "call {}: side {side} re-latched to a new address", show_id(id)),
        }
    }

    /// Bytes in, bytes out: nothing is read, nothing is changed.
    pub(crate) fn relay(&mut self, id: Id, side: usize, data: &[u8], now: Instant) {
        let Some(call) = self.calls.get_mut(&id) else { return };
        call.last = now;
        call.sides[side].packets += 1;
        call.sides[side].bytes += data.len() as u64;
        if let Some(to) = call.sides[1 - side].addr {
            let _ = self.socket.send_to(data, to);
        }
    }

    pub(crate) fn call_housekeeping(&mut self, now: Instant) {
        let idle: Vec<Id> = self
            .calls
            .iter()
            .filter(|(_, c)| now.saturating_duration_since(c.last) >= self.call_idle)
            .map(|(id, _)| *id)
            .collect();
        for id in idle {
            self.release(id, &format!("idle (no datagram for {} s)", self.call_idle.as_secs_f64()));
        }
    }
}
