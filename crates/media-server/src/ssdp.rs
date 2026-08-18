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

pub fn bind_socket() -> Result<Arc<UdpSocket>> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("creating SSDP socket")?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    let addr: SocketAddr = "0.0.0.0:1900".parse().unwrap();
    socket.bind(&addr.into()).context("binding UDP 1900")?;
    socket.set_nonblocking(true)?;
    let socket = UdpSocket::from_std(socket.into())?;
    socket
        .join_multicast_v4(Ipv4Addr::new(239, 255, 255, 250), Ipv4Addr::UNSPECIFIED)
        .context("joining SSDP multicast group")?;
    Ok(Arc::new(socket))
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

async fn notify(socket: &UdpSocket, uuid: &str, location: &str, alive: bool) {
    let nts = if alive { "ssdp:alive" } else { "ssdp:byebye" };
    for (nt, usn) in targets(uuid) {
        let msg = format!(
            "NOTIFY * HTTP/1.1\r\nHOST: {SSDP_ADDR}\r\nCACHE-CONTROL: max-age={MAX_AGE}\r\nLOCATION: {location}\r\nNT: {nt}\r\nNTS: {nts}\r\nSERVER: {SERVER_ID}\r\nUSN: {usn}\r\n\r\n"
        );
        let addr: SocketAddr = SSDP_ADDR.parse().unwrap();
        if let Err(err) = socket.send_to(msg.as_bytes(), addr).await {
            tracing::warn!("SSDP notify failed: {err}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Periodic ssdp:alive announcements (with an initial double burst).
pub async fn alive_loop(socket: Arc<UdpSocket>, uuid: String, location: String) {
    notify(&socket, &uuid, &location, true).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    notify(&socket, &uuid, &location, true).await;
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
    interval.tick().await; // consume the immediate first tick
    loop {
        interval.tick().await;
        notify(&socket, &uuid, &location, true).await;
    }
}

pub async fn byebye(socket: &UdpSocket, uuid: &str, location: &str) {
    notify(socket, uuid, location, false).await;
}
