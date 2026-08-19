use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

const SSDP_ADDR: &str = "239.255.255.250:1900";
const MAX_AGE: u32 = 1800;
const SERVER_ID: &str = "Darwin/UPnP/1.0 RustMediaServer/0.1";

/// The notification targets a MediaServer:1 advertises, as (NT, USN) pairs.
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

/// Receiver: one socket on 0.0.0.0:1900, joined to the SSDP group on every
/// announce interface so M-SEARCH is heard from each attached network.
pub fn bind_socket(interfaces: &[Ipv4Addr]) -> Result<Arc<UdpSocket>> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("creating SSDP socket")?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    let addr: SocketAddr = "0.0.0.0:1900".parse().unwrap();
    socket.bind(&addr.into()).context("binding UDP 1900")?;
    socket.set_nonblocking(true)?;
    let socket = UdpSocket::from_std(socket.into())?;
    let group = Ipv4Addr::new(239, 255, 255, 250);
    if interfaces.is_empty() {
        socket
            .join_multicast_v4(group, Ipv4Addr::UNSPECIFIED)
            .context("joining SSDP multicast group")?;
    } else {
        for iface in interfaces {
            if let Err(err) = socket.join_multicast_v4(group, *iface) {
                tracing::warn!("could not join SSDP group on {iface}: {err}");
            }
        }
    }
    Ok(Arc::new(socket))
}

/// Announce senders: one socket per interface with IP_MULTICAST_IF set, so
/// NOTIFY goes out on every attached network (all advertising the same
/// canonical LOCATION).
pub fn make_senders(interfaces: &[Ipv4Addr]) -> Result<Arc<Vec<(Ipv4Addr, UdpSocket)>>> {
    let mut senders = Vec::new();
    for iface in interfaces {
        let make = || -> Result<UdpSocket> {
            let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
            socket.set_multicast_if_v4(iface)?;
            let bind: SocketAddr = SocketAddr::new((*iface).into(), 0);
            socket.bind(&bind.into())?;
            socket.set_nonblocking(true)?;
            Ok(UdpSocket::from_std(socket.into())?)
        };
        match make() {
            Ok(socket) => senders.push((*iface, socket)),
            Err(err) => tracing::warn!("no SSDP announcements on {iface}: {err}"),
        }
    }
    Ok(Arc::new(senders))
}

/// Answer M-SEARCH queries forever.
pub async fn respond_loop(socket: Arc<UdpSocket>, uuid: String, location: String) {
    let mut buf = [0u8; 2048];
    loop {
        let (len, src) = match socket.recv_from(&mut buf).await {
            Ok(x) => x,
            Err(err) => {
                tracing::warn!("SSDP recv error: {err}");
                continue;
            }
        };
        let Ok(text) = std::str::from_utf8(&buf[..len]) else { continue };
        if !text.starts_with("M-SEARCH") {
            continue;
        }
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
                if let Err(err) = socket.send_to(response.as_bytes(), src).await {
                    tracing::debug!("SSDP response to {src} failed: {err}");
                }
            }
        }
    }
}

async fn notify(
    senders: &[(Ipv4Addr, UdpSocket)],
    uuid: &str,
    location: &str,
    alive: bool,
) {
    let nts = if alive { "ssdp:alive" } else { "ssdp:byebye" };
    let addr: SocketAddr = SSDP_ADDR.parse().unwrap();
    for (nt, usn) in targets(uuid) {
        let msg = format!(
            "NOTIFY * HTTP/1.1\r\nHOST: {SSDP_ADDR}\r\nCACHE-CONTROL: max-age={MAX_AGE}\r\nLOCATION: {location}\r\nNT: {nt}\r\nNTS: {nts}\r\nSERVER: {SERVER_ID}\r\nUSN: {usn}\r\n\r\n"
        );
        for (iface, socket) in senders {
            if let Err(err) = socket.send_to(msg.as_bytes(), addr).await {
                tracing::debug!("SSDP notify on {iface} failed: {err}");
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Periodic ssdp:alive announcements. Each cycle sends the set twice a
/// beat apart — multicast is unacknowledged and lossy (wifi especially),
/// so a single datagram per cycle leaves clients waiting a whole interval
/// when one drops.
pub async fn alive_loop(
    senders: Arc<Vec<(Ipv4Addr, UdpSocket)>>,
    uuid: String,
    location: String,
    every_secs: u64,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(every_secs.max(30)));
    loop {
        interval.tick().await; // first tick fires immediately (startup burst)
        notify(&senders, &uuid, &location, true).await;
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        notify(&senders, &uuid, &location, true).await;
    }
}

pub async fn byebye(senders: &[(Ipv4Addr, UdpSocket)], uuid: &str, location: &str) {
    notify(senders, uuid, location, false).await;
}
