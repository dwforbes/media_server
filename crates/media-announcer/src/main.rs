//! SSDP relay beacon: announces a media-server running on another network
//! segment, for topologies where the server's own multicast doesn't reach
//! clients reliably (e.g. the server's leg onto this network is tenuous
//! wifi, while this host is wired and always on).
//!
//! It reads the same media-server.toml (advertise_ip is required — that's
//! the canonical address clients connect to), periodically health-checks
//! the server by fetching its device description, extracts the device
//! UUID from it (announcements must carry the server's own identity), and
//! while healthy sends the standard ssdp:alive set on this host's
//! networks and answers M-SEARCH queries. When the server disappears it
//! sends ssdp:byebye once and falls silent until it returns.
//!
//! The SSDP wire format here intentionally mirrors media-server/src/ssdp.rs.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use serde::Deserialize;
use socket2::{Domain, Protocol, Socket, Type};

const SSDP_GROUP: &str = "239.255.255.250:1900";
const MAX_AGE: u32 = 1800;
const SERVER_ID: &str = "Darwin/UPnP/1.0 RustMediaServer/0.1 (announcer)";

#[derive(Parser)]
#[command(about = "SSDP relay beacon for a media-server on another network segment")]
struct Args {
    /// The media-server's config file; advertise_ip and bind provide the
    /// canonical URL to announce.
    #[arg(long, default_value = "media-server.toml")]
    config: PathBuf,
    /// Seconds between health checks / announcements.
    #[arg(long, default_value_t = 120)]
    interval_secs: u64,
}

/// The slice of media-server.toml this tool needs (unknown fields ignored).
#[derive(Deserialize)]
struct ServerConfig {
    advertise_ip: Option<IpAddr>,
    #[serde(default = "default_bind")]
    bind: SocketAddr,
}

fn default_bind() -> SocketAddr {
    "0.0.0.0:8200".parse().unwrap()
}

fn targets(uuid: &str) -> Vec<(String, String)> {
    let udn = format!("uuid:{uuid}");
    let mut out: Vec<(String, String)> = [
        "upnp:rootdevice",
        "urn:schemas-upnp-org:device:MediaServer:1",
        "urn:schemas-upnp-org:service:ContentDirectory:1",
        "urn:schemas-upnp-org:service:ConnectionManager:1",
    ]
    .iter()
    .map(|nt| (nt.to_string(), format!("{udn}::{nt}")))
    .collect();
    out.push((udn.clone(), udn));
    out
}

fn local_interfaces() -> Vec<Ipv4Addr> {
    if_addrs::get_if_addrs()
        .map(|ifs| {
            ifs.into_iter()
                .filter_map(|i| match i.ip() {
                    IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local() => Some(v4),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One multicast sender socket per interface.
fn make_senders(interfaces: &[Ipv4Addr]) -> Vec<(Ipv4Addr, UdpSocket)> {
    let mut senders = Vec::new();
    for iface in interfaces {
        let make = || -> Result<UdpSocket> {
            let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
            socket.set_multicast_if_v4(iface)?;
            socket.bind(&SocketAddr::new((*iface).into(), 0).into())?;
            Ok(socket.into())
        };
        match make() {
            Ok(s) => senders.push((*iface, s)),
            Err(err) => tracing::warn!("no announcements on {iface}: {err}"),
        }
    }
    senders
}

fn notify(senders: &[(Ipv4Addr, UdpSocket)], uuid: &str, location: &str, alive: bool) {
    // A relay that cannot send is worse than none — surface the first
    // failure loudly (macOS Local Network privacy denies background
    // processes with "operation not permitted" while logs look healthy).
    static SEND_FAILURE_WARNED: std::sync::Once = std::sync::Once::new();
    let nts = if alive { "ssdp:alive" } else { "ssdp:byebye" };
    let group: SocketAddr = SSDP_GROUP.parse().unwrap();
    for (nt, usn) in targets(uuid) {
        let msg = format!(
            "NOTIFY * HTTP/1.1\r\nHOST: {SSDP_GROUP}\r\nCACHE-CONTROL: max-age={MAX_AGE}\r\nLOCATION: {location}\r\nNT: {nt}\r\nNTS: {nts}\r\nSERVER: {SERVER_ID}\r\nUSN: {usn}\r\n\r\n"
        );
        for (iface, socket) in senders {
            if let Err(err) = socket.send_to(msg.as_bytes(), group) {
                SEND_FAILURE_WARNED.call_once(|| {
                    tracing::warn!(
                        "SSDP send on {iface} failed: {err} — announcements are NOT \
                         going out. On macOS, grant this binary Local Network \
                         permission (System Settings > Privacy & Security)."
                    );
                });
                tracing::debug!("notify on {iface} failed: {err}");
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Health check that doubles as identity discovery: a reachable server's
/// device.xml carries its UDN.
fn fetch_uuid(client: &reqwest::blocking::Client, location: &str) -> Option<String> {
    let body = client.get(location).send().ok()?.text().ok()?;
    let start = body.find("<UDN>uuid:")? + "<UDN>uuid:".len();
    let end = body[start..].find("</UDN>")? + start;
    Some(body[start..end].trim().to_string())
}

/// Answer M-SEARCH while the server is healthy.
fn responder_thread(state: Arc<Mutex<Option<String>>>, location: String, interfaces: Vec<Ipv4Addr>) {
    let make = || -> Result<UdpSocket> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_reuse_address(true)?;
        #[cfg(unix)]
        socket.set_reuse_port(true)?;
        socket.bind(&"0.0.0.0:1900".parse::<SocketAddr>().unwrap().into())?;
        let socket: UdpSocket = socket.into();
        let group = Ipv4Addr::new(239, 255, 255, 250);
        for iface in &interfaces {
            let _ = socket.join_multicast_v4(&group, iface);
        }
        Ok(socket)
    };
    let socket = match make() {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!("no M-SEARCH responder (bind 1900 failed: {err}); announcements only");
            return;
        }
    };
    let mut buf = [0u8; 2048];
    loop {
        let Ok((len, src)) = socket.recv_from(&mut buf) else { continue };
        let Ok(text) = std::str::from_utf8(&buf[..len]) else { continue };
        if !text.starts_with("M-SEARCH") {
            continue;
        }
        let Some(uuid) = state.lock().unwrap().clone() else { continue };
        let st = text
            .lines()
            .find_map(|l| l.strip_prefix("ST:").or_else(|| l.strip_prefix("st:")))
            .map(str::trim)
            .unwrap_or("");
        for (nt, usn) in targets(&uuid) {
            if st == "ssdp:all" || st == nt {
                let response = format!(
                    "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age={MAX_AGE}\r\nEXT:\r\nLOCATION: {location}\r\nSERVER: {SERVER_ID}\r\nST: {nt}\r\nUSN: {usn}\r\n\r\n"
                );
                let _ = socket.send_to(response.as_bytes(), src);
            }
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    let cfg: ServerConfig = toml::from_str(
        &std::fs::read_to_string(&args.config)
            .with_context(|| format!("reading {}", args.config.display()))?,
    )
    .with_context(|| format!("parsing {}", args.config.display()))?;
    let Some(ip) = cfg.advertise_ip else {
        bail!(
            "advertise_ip is not set in {} — the relay needs the server's \
             canonical address",
            args.config.display()
        );
    };
    let location = format!("http://{ip}:{}/device.xml", cfg.bind.port());
    let interfaces = local_interfaces();
    tracing::info!(
        "relaying for {location}, announcing on [{}]",
        interfaces.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ")
    );
    let senders = make_senders(&interfaces);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    // None = server currently unreachable.
    let state: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    {
        let (state, location, interfaces) = (state.clone(), location.clone(), interfaces.clone());
        std::thread::spawn(move || responder_thread(state, location, interfaces));
    }

    loop {
        match fetch_uuid(&client, &location) {
            Some(uuid) => {
                let was = state.lock().unwrap().replace(uuid.clone());
                if was.as_deref() != Some(&uuid) {
                    tracing::info!("server healthy (uuid {uuid}); announcing");
                }
                notify(&senders, &uuid, &location, true);
                std::thread::sleep(Duration::from_millis(400));
                notify(&senders, &uuid, &location, true);
            }
            None => {
                if let Some(uuid) = state.lock().unwrap().take() {
                    tracing::warn!("server unreachable; sending byebye");
                    notify(&senders, &uuid, &location, false);
                }
            }
        }
        std::thread::sleep(Duration::from_secs(args.interval_secs.max(15)));
    }
}
