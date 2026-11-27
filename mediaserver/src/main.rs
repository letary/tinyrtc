//! `tinyrtc-mediaserver`: the command line of `tinyrtc_mediaserver::Server` (lib.rs says what it is),
//! and the events of the server as plain text on stderr.
//!
//! Exit 2 = a wrong command line (clap's message, with the usage), 1 = it could not run or stopped
//! running, 0 = stopped by SIGTERM or Ctrl-C (every connection closed with a goodbye first).

use std::net::{IpAddr, SocketAddr};
use std::process::ExitCode;
use std::time::Duration;

use clap::builder::RangedU64ValueParser;
use clap::error::ErrorKind;
use clap::{CommandFactory, Parser};
use tinyrtc_mediaserver::{Config, Server, DEFAULT_HTTP, DEFAULT_UDP};
use tracing::{error, info};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::SubscriberExt;

/// tinyrtc-mediaserver: rooms and the blind relay of 1:1 calls, behind one UDP port
///
/// Every option is also an environment variable, TINYRTC_<OPTION> (--public-ip =
/// TINYRTC_PUBLIC_IP); the command line wins.
#[derive(Parser)]
#[command(name = "tinyrtc-mediaserver", args_override_self = true)]
struct Args {
    /// The address phones send to, put into every candidate (default: the IP of --udp, when it
    /// names one)
    #[arg(long, value_name = "ip", env = "TINYRTC_PUBLIC_IP")]
    public_ip: Option<IpAddr>,
    /// The UDP socket of both modes (3478: the STUN / TURN port)
    #[arg(long, value_name = "ip:port", env = "TINYRTC_UDP", default_value_t = DEFAULT_UDP)]
    udp: SocketAddr,
    /// The private API
    #[arg(long, value_name = "ip:port", env = "TINYRTC_HTTP", default_value_t = DEFAULT_HTTP)]
    http: SocketAddr,
    /// The API's bearer token; the environment is better (a command line is visible to every
    /// user of the machine)
    #[arg(long, value_name = "s", env = "TINYRTC_SECRET", hide_env_values = true)]
    secret: Option<String>,
    /// A room connection without a packet this long is closed (30)
    #[arg(long, value_name = "seconds", env = "TINYRTC_CONNECTION_IDLE", value_parser = seconds)]
    connection_idle: Option<Duration>,
    /// A room without a participant this long is deleted (300)
    #[arg(long, value_name = "seconds", env = "TINYRTC_ROOM_EMPTY", value_parser = seconds)]
    room_empty: Option<Duration>,
    /// A call without a datagram this long is released (60)
    #[arg(long, value_name = "seconds", env = "TINYRTC_CALL_IDLE", value_parser = seconds)]
    call_idle: Option<Duration>,
    /// What a subscribe connection's estimate probes up to (2500)
    #[arg(long, value_name = "kbps", env = "TINYRTC_SUBSCRIBER_KBPS", value_parser = RangedU64ValueParser::<u64>::new().range(1..))]
    subscriber_kbps: Option<u64>,
    /// The UDP socket's kernel buffers, each way (4096); Linux caps them at net.core.rmem_max /
    /// wmem_max
    #[arg(long, value_name = "KiB", env = "TINYRTC_SOCKET_BUFFER", value_parser = RangedU64ValueParser::<usize>::new().range(1..))]
    socket_buffer: Option<usize>,
    /// What goes to stderr: a level (off error warn info debug trace) for every crate, or per
    /// crate (warn,tinyrtc_mediaserver=info)
    #[arg(long, value_name = "filter", env = "TINYRTC_LOG", default_value = "info", value_parser = log_filter)]
    log_level: Targets,
}

fn main() -> ExitCode {
    drop_empty_env();
    let args = Args::parse();
    // The filter is the only one (fmt's own builder would cap everything at INFO).
    let stderr = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    let subscriber = tracing_subscriber::registry().with(stderr).with(args.log_level.clone());
    // It fails only when a subscriber is set already, and none is.
    let _ = tracing::subscriber::set_global_default(subscriber);

    let Some(secret) = args.secret.filter(|s| !s.is_empty()) else {
        usage(ErrorKind::MissingRequiredArgument, "no secret: --secret or TINYRTC_SECRET")
    };
    let public_ip = match args.public_ip {
        Some(ip) => ip,
        None if !args.udp.ip().is_unspecified() => args.udp.ip(),
        None => usage(ErrorKind::MissingRequiredArgument, "--public-ip (or TINYRTC_PUBLIC_IP) is needed when --udp binds every interface"),
    };
    if public_ip.is_unspecified() {
        usage(ErrorKind::InvalidValue, "--public-ip cannot be the unspecified address");
    }

    let mut config = Config::new(public_ip, secret);
    config.udp = args.udp;
    config.http = args.http;
    config.connection_idle = args.connection_idle.unwrap_or(config.connection_idle);
    config.room_empty = args.room_empty.unwrap_or(config.room_empty);
    config.call_idle = args.call_idle.unwrap_or(config.call_idle);
    config.subscriber_bitrate = args.subscriber_kbps.map_or(config.subscriber_bitrate, |k| k.saturating_mul(1000));
    config.socket_buffer = args.socket_buffer.map_or(config.socket_buffer, |k| k.saturating_mul(1024));
    let server = match Server::start(config) {
        Ok(server) => server,
        Err(e) => {
            error!("cannot start: {e}");
            return ExitCode::FAILURE;
        }
    };
    server.stop_on(terminated());
    if server.wait() {
        ExitCode::SUCCESS
    } else {
        error!("the network thread died");
        ExitCode::FAILURE
    }
}

/// An empty TINYRTC_* variable (a compose file's unset `${VAR}`, say) is an option left out, not an
/// empty value clap would refuse. Called before any thread starts.
fn drop_empty_env() {
    for (name, value) in std::env::vars_os() {
        if value.is_empty() && name.to_str().is_some_and(|n| n.starts_with("TINYRTC_")) {
            std::env::remove_var(name);
        }
    }
}

/// A wrong command line found after parsing: clap's message and usage, exit 2.
fn usage(kind: ErrorKind, message: &str) -> ! {
    Args::command().error(kind, message).exit()
}

fn seconds(text: &str) -> Result<Duration, String> {
    let secs: f64 = text.parse().map_err(|_| "not a number of seconds".to_owned())?;
    let duration = (secs.is_finite() && secs > 0.0).then(|| Duration::try_from_secs_f64(secs).ok()).flatten();
    duration.ok_or_else(|| "not a positive number of seconds".to_owned())
}

/// tracing-subscriber's `target=level` list. An empty one is the default, not silence.
fn log_filter(text: &str) -> Result<Targets, String> {
    if text.trim().is_empty() {
        return Ok(Targets::new().with_default(LevelFilter::INFO));
    }
    // A word alone must be a level: tracing-subscriber takes any other for a target at TRACE,
    // so a misspelt level would silence everything else.
    for word in text.split(',').filter(|d| !d.contains('=')) {
        word.trim().parse::<LevelFilter>().map_err(|_| format!("{word:?} is not a level (off error warn info debug trace)"))?;
    }
    text.parse().map_err(|e| format!("{e}"))
}

/// SIGTERM (a container being stopped) or Ctrl-C.
async fn terminated() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let term = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => drop(term.recv().await),
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => info!("Ctrl-C: stopping"),
        () = term => info!("SIGTERM: stopping"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_line_is_well_formed() {
        Args::command().debug_assert();
    }

    /// Parsing reads the environment, and one test writes it: one at a time.
    static ENVIRONMENT: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn the_environment_names_an_option_and_the_command_line_wins() {
        let _alone = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("TINYRTC_PUBLIC_IP", "192.0.2.7");
        std::env::set_var("TINYRTC_SUBSCRIBER_KBPS", "900");
        std::env::set_var("TINYRTC_SOCKET_BUFFER", "");
        drop_empty_env();
        let args = Args::try_parse_from(["x"]).unwrap();
        assert_eq!(args.public_ip, Some("192.0.2.7".parse().unwrap()));
        assert_eq!(args.subscriber_kbps, Some(900));
        assert_eq!(args.socket_buffer, None);
        let args = Args::try_parse_from(["x", "--public-ip", "192.0.2.8"]).unwrap();
        assert_eq!(args.public_ip, Some("192.0.2.8".parse().unwrap()));
        std::env::remove_var("TINYRTC_PUBLIC_IP");
        std::env::remove_var("TINYRTC_SUBSCRIBER_KBPS");
    }

    #[test]
    fn a_later_option_wins_and_bad_values_are_refused() {
        let _alone = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
        let args = Args::try_parse_from(["x", "--udp", "192.0.2.1:1", "--udp", "192.0.2.2:2", "--call-idle", "0.5"]).unwrap();
        assert_eq!(args.udp, "192.0.2.2:2".parse::<SocketAddr>().unwrap());
        assert_eq!(args.call_idle, Some(Duration::from_millis(500)));
        for bad in [
            ["--connection-idle", "0"],
            ["--call-idle", "-1"],
            ["--room-empty", "inf"],
            ["--room-empty", "1e30"],
            ["--subscriber-kbps", "0"],
            ["--socket-buffer", "x"],
            ["--udp", "192.0.2.1"],
            ["--log-level", "loud"],
            ["--nothing", "1"],
        ] {
            assert!(Args::try_parse_from(["x"].into_iter().chain(bad)).is_err(), "{bad:?}");
        }
    }
}
