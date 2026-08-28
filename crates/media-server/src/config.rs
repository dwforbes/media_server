use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Defaults to ~/Library/Application Support/mediaserver/media.db
    pub db_path: Option<PathBuf>,
    /// HTTP listen address, e.g. "0.0.0.0:8200".
    #[serde(default = "default_bind")]
    pub bind: SocketAddr,
    /// LAN IP advertised in SSDP and media URLs — the canonical address
    /// clients connect to. Auto-detected when unset.
    pub advertise_ip: Option<IpAddr>,
    /// Local interface IPv4 addresses to announce SSDP on. On a multi-homed
    /// host (wired + wifi on different networks) announcements go out on
    /// each, all pointing at the canonical advertise_ip. Defaults to every
    /// non-loopback IPv4 interface.
    pub ssdp_addrs: Option<Vec<std::net::Ipv4Addr>>,
    #[serde(default = "default_name")]
    pub friendly_name: String,
    /// Optional custom device icon (PNG, ideally 120x120) shown by clients
    /// next to the server name. A built-in icon is used when unset.
    pub icon_png: Option<PathBuf>,
    /// How many items the "Recently Added" views list per media type.
    #[serde(default = "default_recent_count")]
    pub recent_count: usize,
    /// Optional HTTPS listener for the web pages on a second port. The
    /// UPnP side stays on the plain `bind` (renderers cannot do TLS).
    pub tls: Option<TlsConfig>,
    /// Seconds between periodic SSDP alive announcements. Lower helps
    /// clients on lossy links (wifi) discover the server sooner.
    #[serde(default = "default_ssdp_alive_secs")]
    pub ssdp_alive_secs: u64,
    /// ffmpeg/ffprobe for on-the-fly extraction of embedded subtitle
    /// tracks (browser playback). Missing binaries just disable that
    /// fallback; sidecar .srt subtitles keep working.
    #[serde(default = "default_ffmpeg")]
    pub ffmpeg_path: String,
    #[serde(default = "default_ffprobe")]
    pub ffprobe_path: String,
    /// Clients to send SSDP announcements to directly (unicast), for
    /// devices whose multicast reception is unreliable. Same cadence and
    /// content as the multicast announcements.
    #[serde(default)]
    pub ssdp_unicast_clients: Vec<std::net::IpAddr>,
}

fn default_ssdp_alive_secs() -> u64 {
    120
}

fn default_ffmpeg() -> String {
    "ffmpeg".into()
}
fn default_ffprobe() -> String {
    "ffprobe".into()
}

fn default_recent_count() -> usize {
    25
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// HTTPS listen address, e.g. "0.0.0.0:8443".
    #[serde(default = "default_tls_bind")]
    pub bind: SocketAddr,
    /// The name the certificate is issued for; used in redirects and in
    /// self-links when a request carries no Host header.
    pub hostname: String,
    /// PEM certificate chain and private key.
    pub cert: PathBuf,
    pub key: PathBuf,
    /// Send browsers that open a page on the plain port to https://hostname.
    /// Only HTML pages: media, artwork, playlists and the UPnP endpoints are
    /// never redirected.
    #[serde(default)]
    pub redirect_pages: bool,
    /// How often to re-read the certificate files, so a renewed certificate
    /// is picked up without a restart.
    #[serde(default = "default_reload_secs")]
    pub reload_secs: u64,
}

fn default_tls_bind() -> SocketAddr {
    "0.0.0.0:8443".parse().unwrap()
}
fn default_reload_secs() -> u64 {
    3600
}

fn default_bind() -> SocketAddr {
    "0.0.0.0:8200".parse().unwrap()
}
fn default_name() -> String {
    "Rust Media Server".into()
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
    }

    pub fn db_path(&self) -> PathBuf {
        self.db_path.clone().unwrap_or_else(media_db::open::default_db_path)
    }

    /// The IP other devices should use to reach us.
    pub fn advertised_ip(&self) -> Result<IpAddr> {
        if let Some(ip) = self.advertise_ip {
            return Ok(ip);
        }
        if !self.bind.ip().is_unspecified() {
            return Ok(self.bind.ip());
        }
        // Routing-table trick: connecting a UDP socket picks the outbound
        // interface without sending a packet.
        let sock = std::net::UdpSocket::bind("0.0.0.0:0")?;
        sock.connect("8.8.8.8:80")?;
        Ok(sock.local_addr()?.ip())
    }

    /// Interfaces to announce SSDP on: the configured list, else every
    /// usable local IPv4 interface.
    pub fn ssdp_interfaces(&self) -> Vec<std::net::Ipv4Addr> {
        if let Some(addrs) = &self.ssdp_addrs {
            return addrs.clone();
        }
        if_addrs::get_if_addrs()
            .map(|ifs| {
                ifs.into_iter()
                    .filter_map(|i| match i.ip() {
                        IpAddr::V4(v4)
                            if !v4.is_loopback() && !v4.is_link_local() =>
                        {
                            Some(v4)
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// The UPnP device UUID must survive restarts so clients recognize us.
/// Stored next to the database.
pub fn load_or_create_uuid(db_path: &Path) -> Result<String> {
    let uuid_path = db_path.with_extension("uuid");
    if let Ok(existing) = std::fs::read_to_string(&uuid_path) {
        let trimmed = existing.trim().to_string();
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
    }
    let fresh = uuid::Uuid::new_v4().to_string();
    if let Some(dir) = uuid_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&uuid_path, &fresh)
        .with_context(|| format!("persisting device uuid to {}", uuid_path.display()))?;
    Ok(fresh)
}
