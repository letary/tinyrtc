# tinyrtc-mediaserver

The media server of tinyrtc: two modes behind ONE public UDP port, and a private HTTP API for the
application's backend. Phones never call the API; they only send media to the UDP port. Signaling
(offers, answers, candidates) is carried by the backend, never by a channel of the server's own.

- **Rooms**: the server is a peer and decrypts. A participant publishes on one connection
  (phone -> server) and subscribes to each other participant on one connection per publisher
  (server -> phone). The phone always offers; the server answers ICE-lite with its public address
  as the only candidate, so one request and its response set a connection up (no trickle).
- **Calls**: a blind relay of a 1:1 call. The server never has a key and reads only the USERNAME
  of a STUN binding request.

```
src/lib.rs    Config, Server::start(config) -> Server (in-process: what the tests run), the events
src/net.rs    the network thread: the socket, the loop, demultiplexing, the timers, the commands
src/room.rs   rooms: participants, the ICE-lite connections, forwarding, keyframe requests
src/call.rs   calls: the reservations, latching, the relay
src/http.rs   the API on axum: the routes, the guard (limits, the secret), the JSON of every body
src/sockbuf.rs  SO_RCVBUF / SO_SNDBUF (std has no setter)
src/main.rs   the command line (clap), the events on stderr, SIGTERM
```

Dependencies: str0m 0.26 (`rust-crypto`, the same patched backend as core) and core by path (its
profile and its STUN parser; the tests' phones are its `Connection`) for the media; around it the
usual crates: axum on tokio for the API, serde / serde_json for its JSON, clap for the command
line, tracing and tracing-subscriber for the events. The media loop is not async: tokio runs the
API only, on two threads of its own. No TLS (the API is plain HTTP on loopback). Its own lock
file; not a workspace with core. Built with stable Rust (the image is).

## Run

```
TINYRTC_SECRET=... tinyrtc-mediaserver --public-ip 203.0.113.10
    --udp 0.0.0.0:3478        both modes' one socket
    --http 127.0.0.1:8080     the private API
    --connection-idle 30 --room-empty 300 --call-idle 60   (seconds)
    --subscriber-kbps 2500    what a subscribe connection's bandwidth estimate probes up to
    --socket-buffer 4096      KiB, the socket's kernel buffers each way
    --log-level info          what goes to stderr (or TINYRTC_LOG): a level, or per crate
                              (warn,tinyrtc_mediaserver=info)
```

Every option is also an environment variable, `TINYRTC_` and its name in capitals
(`TINYRTC_PUBLIC_IP`, `TINYRTC_UDP`, `TINYRTC_HTTP`, `TINYRTC_CONNECTION_IDLE`, `TINYRTC_ROOM_EMPTY`,
`TINYRTC_CALL_IDLE`, `TINYRTC_SUBSCRIBER_KBPS`, `TINYRTC_SOCKET_BUFFER`; the two with names of
their own: `TINYRTC_SECRET`, `TINYRTC_LOG`). The command line wins; an empty variable is an option
left out.

UDP 3478 by default: the port registered for STUN / TURN, the one a network that lets WebRTC
through at all most often leaves open, and an unprivileged one. 443/udp gets through stricter
networks but needs `CAP_NET_BIND_SERVICE` and collides with HTTP/3.

Exit 2 = a wrong command line (clap's message and usage), a missing secret among them; prefer the
environment to `--secret` (a command line is visible to every user of the machine; `--help` does
not show the environment's value). Exit 1 = it could not bind, or its network thread died.
SIGTERM or Ctrl-C closes every room connection with a goodbye (the phones see it at once) and
exits 0. One line per event on stderr, tracing's plain text (time, level, module, the event): a
room, a participant, a connection, a call: created, connected, closed and why; nothing per
packet. `info` also lets the libraries' warnings through (str0m's: it says nothing at info).

**Linux: raise the socket buffers.** Every datagram of every room and call waits in the one
socket's receive buffer while the network thread works. With the default buffers, a load of
10 rooms (1 publisher + 3 subscribers, ~1 Mbit/s each) and 5 calls lost a third of the rooms'
frames and ~10% of the calls' (measured on Windows, 64 KiB default); with 4 MiB, nothing. Linux
caps the size asked at `net.core.rmem_max` / `wmem_max` (212992 by default):

```
sysctl -w net.core.rmem_max=8388608 net.core.wmem_max=8388608
```

The start log says what the kernel gave (`udp buffers: receive ... KiB`).

## Deploy

A release publishes the image `ghcr.io/letary/tinyrtc-mediaserver` (`:X.Y.Z`, `:X.Y`, `:latest`;
the Dockerfile here builds it, from the repository root). With Docker Compose:

```yaml
services:
  mediaserver:
    image: ghcr.io/letary/tinyrtc-mediaserver:0.1.1
    restart: unless-stopped
    # The host's network: media is UDP, and Docker's port publishing forwards every datagram
    # through a userland proxy and rewrites its source address.
    network_mode: host
    env_file: .env
```

and beside it the `.env`, the server's whole configuration:

```
TINYRTC_SECRET=<a long random string: openssl rand -hex 32>
TINYRTC_PUBLIC_IP=<the address phones send to>
# TINYRTC_UDP=0.0.0.0:3478      the defaults
# TINYRTC_HTTP=127.0.0.1:8080
```

(Environment variables since 0.1.1; 0.1.0 takes all but the secret as a `command:`.) On the host:
the socket buffers above (a container does not change them), and the firewall open for UDP 3478 —
never for 8080.

**The API behind a reverse proxy.** It is plain HTTP with a bearer secret and listens on loopback:
a backend on the same machine calls it as it is. For a backend elsewhere, put TLS in front of it
and let only that backend in — nginx:

```nginx
server {
    listen 443 ssl;
    server_name rtc.example.com;
    ssl_certificate     /etc/letsencrypt/live/rtc.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/rtc.example.com/privkey.pem;

    allow 198.51.100.7;   # the backend
    deny all;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        client_max_body_size 256k;   # the API's own limit
        proxy_read_timeout 15s;
    }
}
```

The `Authorization` header passes through: the secret is still checked here. **The UDP port is not
proxied**: media is already encrypted end to end (DTLS, SRTP), a proxy only adds a hop to every
datagram, and one that tracks UDP "sessions" drops the mapping of a call that went quiet.

## The API

`Authorization: Bearer <secret>` on every request (401 otherwise, whatever the path). JSON unless
noted; errors are `{"error": "..."}` with 400 (malformed, bad SDP), 401, 404, 405, 409, 413 (a
body over 256 KiB), 408 (a body that took over 10 s), 503 (64 requests at once already, or the
network thread did not answer within 10 s). One request per connection (`Connection: close`).

```
POST   /calls                     {"ufrags": ["<a>", "<b>"]}
                                  -> 201 {"id", "candidate": "candidate:1 1 udp 255 <ip> <port> typ relay raddr 0.0.0.0 rport 0"}
                                  409 if the pair is reserved already
GET    /calls/{id}                -> {"sides": [{"ufrag", "connected", "packets", "bytes"} x2], "idle_ms"}
DELETE /calls/{id}                -> 204
POST   /rooms                     -> 201 {"id"}
GET    /rooms/{id}                -> {"participants": [{"id", "publishing", "connected", "subscribers": [..],
                                                       "bytes_in", "bytes_out"}]}
DELETE /rooms/{id}                -> 204
POST   /rooms/{id}/publish/{user}               the offer (SDP text) -> the answer (SDP text)
POST   /rooms/{id}/subscribe/{user}/{other}     the offer -> the answer; 404 if {other} is not publishing
DELETE /rooms/{id}/participants/{user}          -> 204
```

A participant id is 1..64 of `A-Z a-z 0-9 . _ ~ -`. A participant exists while it has a
connection; `connected` = one of its connections is; `bytes_in` / `bytes_out` = what crossed the
UDP port from / to that phone, closed connections included. A call side is `connected` once
latched; its `packets` / `bytes` are what came from it.

### Rooms, from the backend

1. `POST /rooms`.
2. Each phone: a connection, `offer()`, `POST /rooms/{id}/publish/{me}` with it, `accept_answer()`
   with the response. A second publish by the same participant replaces the first: its
   subscribers stay, and are fed from the new connection from its first keyframe.
3. For each other publisher: a connection, `offer()`, `POST /rooms/{id}/subscribe/{me}/{other}`.
4. Leaving: `DELETE /rooms/{id}/participants/{me}` closes its publish connection, its
   subscriptions and everyone's subscriptions of it. A phone that vanishes is dropped after
   `--connection-idle`; then everyone's subscriptions of it close, which is how the phones learn
   it left when the backend is late.

A room connection is not `relay_only`: the answer's candidate is the server's own (`typ host`:
the server is the peer here), which a relay-only connection does not take — it would never
connect. Nothing is lost: the other participants never learn a phone's address in a room (they
are never its peers); the server sees it, as a relay would.

### Calls, from the backend

1. The offer and the answer cross between the phones as usual, and so do their candidates. A
   phone that must not reveal its address is `relay_only` (core; `relayOnly` in the plugin): its
   descriptions carry no address, it trickles none, and it checks no candidate of the other side
   but a `typ relay` one — whatever it is given. A call with one relay-only side is a relay-only
   call; the other side may be a plain connection.
2. The backend reads the two `a=ice-ufrag:` values (the offer's, the answer's) and
   `POST /calls {"ufrags": [offer's, answer's]}`.
3. Both phones are given the response's `candidate` with `add_candidate`.
4. An ICE restart has new ufrags: the reservation of the old ones no longer latches anything, so
   the backend reserves again with the new pair (and may `DELETE` the old one). The candidate is
   the same line.

**The candidate** is the server's public address typed `relay`, with priority 255 — the lowest
the RFC 8445 formula gives (type preference 0, local preference 0) — and an empty related address
(`raddr 0.0.0.0 rport 0`, `::` for IPv6). Relay-typed because a relay-only connection takes no
other kind (so an app cannot open a direct path by passing the other side's candidates too);
the lowest priority because a connection that is not relay-only takes it as one candidate among
its others, and every direct pair must outrank it: a call that can go direct still does, and
the relay carries only its checks. core and Chrome take the line (core/tests/blind_relay.rs;
tools/peer `node interop.mjs --core --relay`).

## How it works

**One network thread** owns the socket, every str0m `Rtc` and the call table. It waits for one
thing (a datagram, with the earliest deadline as the read timeout) and applies one input: a
command of the API, a due timer, a datagram. Each mutation of an `Rtc` is followed by polling it
dry (str0m's rule); what the poll brings out for other connections (a frame to forward, a
keyframe to ask for, a close) is queued and applied after, one at a time. The API sends commands
through a channel and wakes the thread with a one-byte datagram from a loopback socket of its own;
neither blocks, and each command carries a oneshot reply that the request's task awaits.
A panic while handling a connection (str0m's or ours) is caught and costs that connection only.

**Demultiplexing**, in this order: (a) a source address already latched goes to its room
connection or call side; (b) a STUN binding request goes to the room connection whose ICE ufrag
is the first half of its USERNAME (confirmed by `rtc.accepts`), else to the call reserved for the
USERNAME pair; (c) anything else from an unknown address is dropped. A binding request from a
latched address whose USERNAME is someone else's (a NAT that reused a mapping) goes through (b).

**Rooms** forward frame by frame (str0m's frame level, as its `chat.rs`): each hop repairs its own
losses with NACK / RTX; payload types are matched by codec parameters, not number; the video
orientation and audio level extensions are carried over. A video frame the publish hop could not
repair is not passed on: its subscribers wait for the next keyframe. Keyframe requests (a
subscriber's PLI, a new subscriber, a loss upstream) go to the publisher at most once per second;
one that falls inside the second (or before the publisher is connected) is sent when it may be.
Every room connection shares one DTLS certificate made at start; str0m's stats are off.

**Calls**: a binding request from an unknown address whose USERNAME `<receiver>:<sender>` is a
reserved pair latches the sender's address to its side; every datagram from a latched address is
then forwarded unchanged to the other side's address (dropped while it has none). The same pair
from a new address re-latches that side (a phone that moved networks).

## Limits, and what is not here

- **A slow subscriber** gets what its connection's pacer lets through. There is no simulcast and
  no transcoding, and nothing tells the publisher about a subscriber's bandwidth: the publisher's
  bitrate follows its own uplink. A subscriber whose downlink is below it gets queueing (str0m's
  pacer holds up to ~2 s, then drains faster), then loss and keyframe requests; it lowers no one
  else's quality, and its own is not adapted. A subscribe connection's estimate starts at the
  profile's 300 kbit/s and probes up to `--subscriber-kbps`: a publisher above the estimate is
  queued while it ramps up. Seen on loopback at ~1 Mbit/s: 2 s after the start, 1.3-1.8 s of
  frames were still queued (on the publishing phone's hop or the subscriber's: not told apart;
  both start at 300 kbit/s, and the test's phones set no desired bitrate).
- **Data channels in rooms are not forwarded** in this version (a phone's channels open, and go
  nowhere).
- **Calls, security**: only someone who knows BOTH ufrags of a reserved call can latch; one ufrag,
  or none, latches nothing and gets nothing forwarded. MESSAGE-INTEGRITY is not verified (the
  server has no ICE password), so someone who saw both ufrags (they travel in the descriptions,
  through the backend) can take a side's place until that side's next check takes it back. The
  media stays end-to-end encrypted (DTLS-SRTP between the phones): the worst is a cut call, not a
  heard one.
- **NAT rebinding in rooms** follows str0m's ICE-lite: a phone's next check from the new address
  (USE-CANDIDATE, every ~2-3 s) moves the path; media sent from the new address before it is
  dropped. Seen on loopback: frames again ~2 s after the move. Calls re-latch the same way.
- **IPv4** in practice: core's phones bind IPv4 sockets. The server binds whatever `--udp` says,
  one family.
- **One thread**: every room and call shares one core. Measured (`tests/load.rs`, Ryzen 9 9900X,
  Windows 11, loopback, ~1.1 Mbit/s a stream): 60 subscriber streams (~66 Mbit/s out) took ~20%
  of one core; 20 calls (~44 Mbit/s relayed) ~5%; 10 rooms of 1+3 with 5 calls 7-15% (it varies
  run to run), and ~21% under WSL2 Ubuntu on the same machine. A rough ceiling from that: a few
  hundred subscriber streams, or several hundred Mbit/s of relayed calls, per process. Not
  measured on a real NIC, a server CPU, or with real loss.

## Tests

```
cargo test --release                                   every test, in-process on 127.0.0.1
cargo test --release --test calls -- --ignored         + the released call's phones go Disconnected (~25 s)
cargo test --release --test load -- --ignored --nocapture      the measurement above
                                    LOAD_ROOMS / LOAD_SUBSCRIBERS / LOAD_CALLS / LOAD_SECONDS
cargo test --release --test browser -- --ignored --nocapture   Chrome publishes and subscribes
```

The browser test needs node, Chrome and Playwright: `npm install` in `tests/browser`, or
`TINYRTC_PLAYWRIGHT=<a node_modules that has playwright>` (tools/peer's). Run it with node, not
bun (Playwright hangs under Bun on this machine).
