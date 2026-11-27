//! The media server of tinyrtc. Two modes behind ONE public UDP port, and a private HTTP API for
//! the application's backend (phones never call it; they only send media to the UDP port):
//!
//!   ROOMS  the server is a peer: every phone connection (one publish, one subscribe per other
//!          publisher) is a str0m `Rtc` of the fixed profile, answered ICE-lite with the public
//!          address as the only candidate. Media of a publish connection is written into each of
//!          its subscribe connections, frame by frame (room.rs).
//!   CALLS  a blind relay: the two phones' offer / answer go through the backend, which reserves
//!          the pair of ICE ufrags here; a STUN binding request's USERNAME latches a phone's
//!          address to its side, and every datagram from a latched address is forwarded unchanged
//!          to the other side. No key, nothing parsed past the USERNAME (call.rs).
//!
//! Threads: ONE network thread owns the socket, every `Rtc` and the call table (net.rs), and
//! knows nothing of async. The API (http.rs, axum) runs on a small tokio runtime of the server's
//! own; a request sends the network thread one command through a channel and awaits its reply.
//!
//! `Server::start` is the whole server; `main.rs` is its command line. The tests run it in-process
//! on 127.0.0.1 with port 0.

use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, UdpSocket};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// One event: a line of text as a tracing event at `$level` (`INFO`, `WARN`, `ERROR`), and the
/// same text into `Config::log`'s channel. One per event (a room, a participant, a connection, a
/// call: created, connected, closed and why); nothing per packet.
macro_rules! log_line {
    ($log:expr, $level:ident, $($arg:tt)+) => {{
        let line = format!($($arg)+);
        tracing::event!(tracing::Level::$level, "{line}");
        $log.send(line);
    }};
}

mod call;
mod http;
mod net;
mod room;
mod sockbuf;

/// Everything the server is told at start.
#[derive(Debug, Clone)]
pub struct Config {
    /// The address phones send to: what goes into every candidate (with the UDP port bound).
    pub public_ip: IpAddr,
    /// The one UDP socket of both modes.
    pub udp: SocketAddr,
    /// The private API.
    pub http: SocketAddr,
    /// `Authorization: Bearer <secret>` on every request.
    pub secret: String,
    /// A room connection that receives no packet for this long is closed.
    pub connection_idle: Duration,
    /// A room with no participant for this long is deleted.
    pub room_empty: Duration,
    /// A call that relays no datagram for this long is released.
    pub call_idle: Duration,
    /// At most one keyframe request towards a publisher per this long.
    pub keyframe_interval: Duration,
    /// What a subscribe connection's bandwidth estimate probes up to (bits per second): without
    /// it the estimate stays near the profile's start (300 kbit/s) and the pacer holds a
    /// publisher's video back.
    pub subscriber_bitrate: u64,
    /// The UDP socket's kernel buffers, each way (sockbuf.rs: why; Linux caps it at
    /// net.core.rmem_max / wmem_max).
    pub socket_buffer: usize,
    /// Every event's line also goes here (the tests read them), besides tracing (which the
    /// binary writes to stderr).
    pub log: Option<Sender<String>>,
}

/// UDP 3478: the port registered for STUN / TURN, the one a network that lets WebRTC through at
/// all most often leaves open for UDP, and an unprivileged one. (443/udp passes stricter
/// networks but needs CAP_NET_BIND_SERVICE and collides with HTTP/3.)
pub const DEFAULT_UDP: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 3478));
/// The API is private: loopback unless told otherwise.
pub const DEFAULT_HTTP: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8080));

impl Config {
    /// The defaults: 30 s connection idle, 5 min empty room, 60 s call idle.
    pub fn new(public_ip: IpAddr, secret: impl Into<String>) -> Config {
        Config {
            public_ip,
            udp: DEFAULT_UDP,
            http: DEFAULT_HTTP,
            secret: secret.into(),
            connection_idle: Duration::from_secs(30),
            room_empty: Duration::from_secs(5 * 60),
            call_idle: Duration::from_secs(60),
            keyframe_interval: Duration::from_secs(1),
            subscriber_bitrate: 2_500_000,
            socket_buffer: 4 << 20,
            log: None,
        }
    }
}

/// A running server. Dropping it stops it: the network thread is joined and the API's runtime
/// shut down, both ports closed by the time the drop returns (dropped inside an async context,
/// the runtime's threads end in the background instead).
pub struct Server {
    udp: SocketAddr,
    http: SocketAddr,
    public: SocketAddr,
    net: Arc<net::Handle>,
    net_thread: Option<JoinHandle<()>>,
    /// The API's: its tasks, the listener's among them, end with it.
    runtime: Option<tokio::runtime::Runtime>,
}

/// The API's worker threads. A request does little work of its own (it waits for the network
/// thread), and the API is the backend's, a few requests at a time.
const API_THREADS: usize = 2;

impl Server {
    pub fn start(config: Config) -> io::Result<Server> {
        if config.secret.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "no secret"));
        }
        let socket = UdpSocket::bind(config.udp)?;
        let buffers = sockbuf::set(&socket, config.socket_buffer);
        let udp = socket.local_addr()?;
        let public = SocketAddr::new(config.public_ip, udp.port());
        // Bound here, not in the runtime, so that a port in use is this call's error.
        let listener = TcpListener::bind(config.http)?;
        listener.set_nonblocking(true)?;
        let http = listener.local_addr()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(API_THREADS)
            .thread_name("tinyrtc-http")
            .enable_all()
            .build()?;
        let listener = {
            let _inside = runtime.enter();
            tokio::net::TcpListener::from_std(listener)?
        };

        let log = Log(config.log.clone());
        let (handle, network) = net::Net::new(socket, public, &config, log.clone())?;
        let handle = Arc::new(handle);
        let net_thread = thread::Builder::new().name("tinyrtc-net".into()).spawn(move || network.run())?;
        runtime.spawn(http::serve(listener, handle.clone(), config.secret.clone(), log.clone()));

        log_line!(log, INFO, "started: udp {udp} (candidate {public}), http {http}");
        log_line!(
            log,
            INFO,
            "udp buffers: receive {} KiB, send {} KiB (asked {} KiB each)",
            buffers.0 / 1024,
            buffers.1 / 1024,
            config.socket_buffer / 1024
        );
        Ok(Server { udp, http, public, net: handle, net_thread: Some(net_thread), runtime: Some(runtime) })
    }

    /// The UDP socket's bound address.
    pub fn udp_addr(&self) -> SocketAddr {
        self.udp
    }
    /// The address in the candidates: the public IP with the bound UDP port.
    pub fn public_addr(&self) -> SocketAddr {
        self.public
    }
    pub fn http_addr(&self) -> SocketAddr {
        self.http
    }

    /// Stops the server when `signal` resolves; `wait` then returns true. The future runs on
    /// the API's tokio runtime: what it does with tokio once polled (an async fn or block
    /// awaiting `tokio::signal`, main.rs: SIGTERM, Ctrl-C) works there.
    pub fn stop_on(&self, signal: impl Future<Output = ()> + Send + 'static) {
        let net = self.net.clone();
        if let Some(runtime) = &self.runtime {
            runtime.spawn(async move {
                signal.await;
                net.stop();
            });
        }
    }

    /// Blocks until the network thread ends: false = it died (a panic outside every connection's
    /// guard), and the process should exit with an error for its supervisor to restart it.
    pub fn wait(mut self) -> bool {
        match self.net_thread.take() {
            Some(net) => net.join().is_ok(),
            None => true,
        }
    }

    fn stop(&mut self) {
        // The network thread first (it closes every connection with a goodbye), then the API's
        // runtime, its tasks dropped: a request in flight among them, the listener too.
        self.net.stop();
        if let Some(net) = self.net_thread.take() {
            let _ = net.join();
        }
        if let Some(runtime) = self.runtime.take() {
            // Waiting is not allowed inside another runtime; there its threads end on their own.
            if tokio::runtime::Handle::try_current().is_ok() {
                runtime.shutdown_background();
            } else {
                runtime.shutdown_timeout(Duration::from_secs(1));
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Where `log_line!` sends a copy of each line: `Config::log`'s channel, if there is one.
#[derive(Clone)]
pub(crate) struct Log(Option<Sender<String>>);

impl Log {
    pub(crate) fn send(&self, line: String) {
        if let Some(tx) = &self.0 {
            let _ = tx.send(line);
        }
    }
}
