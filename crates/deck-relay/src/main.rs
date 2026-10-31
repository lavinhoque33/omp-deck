//! `omp-deck-relay`: run the relay (`serve`) and manage its enrolled rooms (`enroll`, `list`,
//! `remove`). Exit codes: 0 success, 1 failure, 2 usage error.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, bail};
use deck_relay::{AdmissionRate, Config, ProxyRange, RateLimit, rooms};

const BIN: &str = "omp-deck-relay";
const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "\
usage: omp-deck-relay <command> [options]

commands:
  serve --listen <addr:port> --state-dir <dir> [options]
                                run the relay (SIGINT/SIGTERM, Ctrl+C on Windows: graceful stop)
  enroll --state-dir <dir> [--label <text>]
                                create a room; prints its room key once
  list --state-dir <dir>        list enrolled rooms
  remove --state-dir <dir> <room id>
                                remove a room (its gateway and devices are disconnected)
  --help, --version

serve options:
  --tls-cert <pem> --tls-key <pem>
                                terminate TLS in-process (default: plain, behind a TLS proxy)
  --trust-forwarded-for         key per-IP limits on the last X-Forwarded-For hop of connections
                                from loopback or a --trusted-proxy (plain HTTP only)
  --trusted-proxy <ip or cidr>  a proxy address besides loopback whose X-Forwarded-For counts
                                (repeatable; needs --trust-forwarded-for)
  --web-dir <dir>               the phone app to serve (default: <binary dir>/web, then
                                /usr/share/omp-deck-relay/web; neither: no app)
  --max-connections <n>         connections in total, pending TLS/HTTP included (default 1024)
  --max-connections-per-ip <n>  per client address, IPv6 per /64 (default 32)
  --max-devices-per-room <n>    (default 16)
  --device-admissions-per-sec <n>
                                device connections admitted per room (default 10, burst 2x)
  --device-bytes-per-sec <n>    inbound per device (default 1048576, burst 4x)
  --device-messages-per-sec <n> inbound per device (default 500, burst 2x)
  --gateway-bytes-per-sec <n>   inbound per gateway (default 8388608, burst 2x)
  --gateway-messages-per-sec <n>
                                inbound per gateway (default 4000, burst 2x)
  --queue-messages <n>          outbound queue per connection (default 256)
  --queue-bytes <n>             outbound queue per connection (default 4194304)
  --idle-timeout-secs <n>       close connections silent this long (default 75; pings every 25)

logging: RUST_LOG (default info; tungstenite, rustls and hyper stay at info or above)
";

#[derive(Default)]
struct Args {
    command: Option<String>,
    state_dir: Option<PathBuf>,
    listen: Option<String>,
    label: Option<String>,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    web_dir: Option<PathBuf>,
    trust_forwarded_for: bool,
    trusted_proxies: Vec<ProxyRange>,
    /// `serve` limit overrides, applied in order.
    limits: Vec<(Limit, u64)>,
    positional: Vec<String>,
    version: bool,
}

#[derive(Clone, Copy)]
enum Limit {
    MaxConnections,
    MaxConnectionsPerIp,
    MaxDevicesPerRoom,
    DeviceAdmissions,
    DeviceBytes,
    DeviceMessages,
    GatewayBytes,
    GatewayMessages,
    QueueMessages,
    QueueBytes,
    IdleTimeoutSecs,
}

const LIMIT_FLAGS: [(&str, Limit); 11] = [
    ("--max-connections", Limit::MaxConnections),
    ("--max-connections-per-ip", Limit::MaxConnectionsPerIp),
    ("--max-devices-per-room", Limit::MaxDevicesPerRoom),
    ("--device-admissions-per-sec", Limit::DeviceAdmissions),
    ("--device-bytes-per-sec", Limit::DeviceBytes),
    ("--device-messages-per-sec", Limit::DeviceMessages),
    ("--gateway-bytes-per-sec", Limit::GatewayBytes),
    ("--gateway-messages-per-sec", Limit::GatewayMessages),
    ("--queue-messages", Limit::QueueMessages),
    ("--queue-bytes", Limit::QueueBytes),
    ("--idle-timeout-secs", Limit::IdleTimeoutSecs),
];

fn parse(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut a = Args::default();
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| it.next().ok_or_else(|| format!("{flag} needs a value"));
        match arg.as_str() {
            "--version" | "-V" => a.version = true,
            "--state-dir" => a.state_dir = Some(PathBuf::from(value("--state-dir")?)),
            "--listen" => a.listen = Some(value("--listen")?),
            "--label" => a.label = Some(value("--label")?),
            "--tls-cert" => a.tls_cert = Some(PathBuf::from(value("--tls-cert")?)),
            "--tls-key" => a.tls_key = Some(PathBuf::from(value("--tls-key")?)),
            "--web-dir" => a.web_dir = Some(PathBuf::from(value("--web-dir")?)),
            "--trust-forwarded-for" => a.trust_forwarded_for = true,
            "--trusted-proxy" => {
                a.trusted_proxies.push(
                    value("--trusted-proxy")?
                        .parse()
                        .map_err(|e| format!("--trusted-proxy {e}"))?,
                );
            }
            "-h" | "--help" | "help" => return Err(String::new()),
            s if s.starts_with('-') => {
                let Some(&(name, limit)) = LIMIT_FLAGS.iter().find(|(f, _)| *f == s) else {
                    return Err(format!("unknown option {s}"));
                };
                let v = value(name)?;
                let n: u64 = v
                    .parse()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| format!("{name} {v:?}: expected a positive number"))?;
                a.limits.push((limit, n));
            }
            _ if a.command.is_none() => a.command = Some(arg),
            _ => a.positional.push(arg),
        }
    }
    Ok(a)
}

fn main() {
    let code = match parse(std::env::args().skip(1)) {
        Err(e) => {
            if !e.is_empty() {
                eprintln!("{BIN}: {e}");
            }
            eprint!("{USAGE}");
            if e.is_empty() { 0 } else { 2 }
        }
        Ok(a) if a.version => {
            println!("{BIN} {VERSION}");
            0
        }
        Ok(a) => match run(a) {
            Ok(()) => 0,
            Err(e) if e.is::<Usage>() => {
                eprintln!("{BIN}: {e}");
                eprint!("{USAGE}");
                2
            }
            Err(e) => {
                eprintln!("{BIN}: {e:#}");
                1
            }
        },
    };
    std::process::exit(code);
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Usage(String);

fn usage(msg: impl Into<String>) -> anyhow::Error {
    Usage(msg.into()).into()
}

/// Rejects options the command does not take.
fn only(a: &Args, serve: bool, label: bool, positional: usize) -> anyhow::Result<()> {
    let cmd = a.command.as_deref().unwrap_or("");
    let serve_only = a.listen.is_some()
        || a.tls_cert.is_some()
        || a.tls_key.is_some()
        || a.web_dir.is_some()
        || a.trust_forwarded_for
        || !a.trusted_proxies.is_empty()
        || !a.limits.is_empty();
    if serve_only && !serve {
        return Err(usage(format!("{cmd} does not take serve options")));
    }
    if a.label.is_some() && !label {
        return Err(usage(format!("{cmd} does not take --label")));
    }
    if a.positional.len() != positional {
        return Err(usage(format!("{cmd}: unexpected arguments")));
    }
    Ok(())
}

fn run(a: Args) -> anyhow::Result<()> {
    let Some(command) = a.command.clone() else {
        return Err(usage("missing command"));
    };
    let state_dir = || {
        a.state_dir
            .clone()
            .ok_or_else(|| usage(format!("{command} needs --state-dir")))
    };
    match command.as_str() {
        "serve" => {
            only(&a, true, false, 0)?;
            serve(&a, state_dir()?)
        }
        "enroll" => {
            only(&a, false, true, 0)?;
            let (key, record) = rooms::enroll(&state_dir()?, a.label.as_deref().unwrap_or(""))?;
            println!("room id:  {}", record.room);
            println!("room key: {}", key.encode());
            eprintln!(
                "The room key is shown only once and is not stored here: enter it in OmpDeck with \
                 this relay's URL. Lost keys cannot be recovered; remove the room and enroll again."
            );
            Ok(())
        }
        "list" => {
            only(&a, false, false, 0)?;
            let records = rooms::read_records(&state_dir()?)?;
            if records.is_empty() {
                eprintln!("no rooms enrolled");
            }
            for r in records {
                println!("{}  {}  {}", r.room, utc(r.created), r.label);
            }
            Ok(())
        }
        "remove" => {
            only(&a, false, false, 1)?;
            let id = &a.positional[0];
            if !rooms::remove(&state_dir()?, id)? {
                bail!("no room {id:?} is enrolled");
            }
            println!("removed room {id}");
            Ok(())
        }
        other => Err(usage(format!("unknown command {other:?}"))),
    }
}

/// `burst / rate` of the defaults is kept when a rate is overridden.
fn scaled(rate: u64, default_rate: u64, default_burst: u64) -> u64 {
    rate.saturating_mul(default_burst / default_rate)
}

fn apply_limits(config: &mut Config, limits: &[(Limit, u64)]) {
    let (d, g, a) = (RateLimit::DEVICE, RateLimit::GATEWAY, AdmissionRate::DEVICE);
    for &(limit, n) in limits {
        let size = usize::try_from(n).unwrap_or(usize::MAX);
        match limit {
            Limit::MaxConnections => config.max_connections = size,
            Limit::MaxConnectionsPerIp => config.max_connections_per_ip = size,
            Limit::MaxDevicesPerRoom => config.max_devices_per_room = size,
            Limit::DeviceAdmissions => {
                config.device_admissions.per_sec = n;
                config.device_admissions.burst = scaled(n, a.per_sec, a.burst);
            }
            Limit::DeviceBytes => {
                config.device_rate.bytes_per_sec = n;
                config.device_rate.bytes_burst = scaled(n, d.bytes_per_sec, d.bytes_burst);
            }
            Limit::DeviceMessages => {
                config.device_rate.messages_per_sec = n;
                config.device_rate.messages_burst = scaled(n, d.messages_per_sec, d.messages_burst);
            }
            Limit::GatewayBytes => {
                config.gateway_rate.bytes_per_sec = n;
                config.gateway_rate.bytes_burst = scaled(n, g.bytes_per_sec, g.bytes_burst);
            }
            Limit::GatewayMessages => {
                config.gateway_rate.messages_per_sec = n;
                config.gateway_rate.messages_burst =
                    scaled(n, g.messages_per_sec, g.messages_burst);
            }
            Limit::QueueMessages => config.queue_messages = size,
            Limit::QueueBytes => config.queue_bytes = size,
            Limit::IdleTimeoutSecs => config.idle_timeout = Duration::from_secs(n),
        }
    }
}

fn serve(a: &Args, state_dir: PathBuf) -> anyhow::Result<()> {
    let listen = a
        .listen
        .as_deref()
        .ok_or_else(|| usage("serve needs --listen <addr:port>"))?;
    let listen: SocketAddr = listen
        .parse()
        .map_err(|_| usage(format!("--listen {listen:?}: expected <ip>:<port>")))?;
    if !a.trusted_proxies.is_empty() && !a.trust_forwarded_for {
        return Err(usage("--trusted-proxy needs --trust-forwarded-for"));
    }
    if a.trust_forwarded_for && a.tls_cert.is_some() {
        return Err(usage(
            "--trust-forwarded-for does not go with --tls-cert: with in-process TLS clients \
             connect directly, so no proxy sets X-Forwarded-For",
        ));
    }
    deck_relay::init_logging();

    let mut config = Config::new(state_dir);
    config.trust_forwarded_for = a.trust_forwarded_for;
    config.trusted_proxies = a.trusted_proxies.clone();
    apply_limits(&mut config, &a.limits);
    config.web_dir = a.web_dir.clone().or_else(default_web_dir);
    config.tls = match (&a.tls_cert, &a.tls_key) {
        (Some(cert), Some(key)) => Some(deck_relay::load_tls(cert, key)?),
        (None, None) => None,
        _ => return Err(usage("--tls-cert and --tls-key go together")),
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")?;
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .with_context(|| format!("listening on {listen}"))?;
        deck_relay::serve(config, listener, shutdown_signal()).await?;
        log::info!("stopped");
        Ok(())
    })
}

/// The phone app shipped with the relay: `web/` next to the binary (release archive), then the
/// container image's `/usr/share/omp-deck-relay/web`.
fn default_web_dir() -> Option<PathBuf> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("web")));
    let system = cfg!(unix).then(|| PathBuf::from("/usr/share/omp-deck-relay/web"));
    beside.into_iter().chain(system).find(|dir| dir.is_dir())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
                return;
            }
            Err(e) => log::error!("cannot watch SIGTERM: {e}"),
        }
    }
    if let Err(e) = tokio::signal::ctrl_c().await {
        log::error!("cannot watch Ctrl+C: {e}");
        std::future::pending::<()>().await;
    }
}

/// `YYYY-MM-DD HH:MM UTC` from Unix seconds (Howard Hinnant's civil-from-days).
fn utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60
    )
}
