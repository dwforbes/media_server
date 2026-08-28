mod config;
mod counts;
mod didl;
mod http;
mod objectid;
mod soap;
mod ssdp;
mod tree;
mod xml;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use http::AppState;

#[derive(Parser)]
#[command(about = "UPnP AV MediaServer backed by the shared catalog database")]
struct Args {
    /// Path to the TOML config file.
    #[arg(long, default_value = "media-server.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let cfg = config::Config::load(&args.config)?;
    // rustls needs one process-wide crypto provider; ring is the only one
    // compiled in (no cmake/C toolchain needed on the Pi).
    let _ = rustls::crypto::ring::default_provider().install_default();
    let db_path = cfg.db_path();

    let conn = media_db::open_ro(&db_path)?;
    let uuid = config::load_or_create_uuid(&db_path)?;
    let ip = cfg.advertised_ip()?;
    let base_url = format!("http://{ip}:{}", cfg.bind.port());
    let location = format!("{base_url}/device.xml");

    let icon = match &cfg.icon_png {
        Some(path) => {
            let bytes = std::fs::read(path)
                .with_context(|| format!("reading icon {}", path.display()))?;
            (bytes.clone(), bytes, true)
        }
        None => (
            include_bytes!("../assets/icon-120.png").to_vec(),
            include_bytes!("../assets/icon-48.png").to_vec(),
            false,
        ),
    };

    let state = Arc::new(AppState {
        db: tokio::sync::Mutex::new(conn),
        update_id: AtomicU32::new(1),
        counts: Default::default(),
        uuid: uuid.clone(),
        friendly_name: cfg.friendly_name.clone(),
        base_url: base_url.clone(),
        icon,
        recent_count: cfg.recent_count,
        ffmpeg: cfg.ffmpeg_path.clone(),
        ffprobe: cfg.ffprobe_path.clone(),
        vtt_cache: {
            let dir = db_path
                .parent()
                .map(|d| d.join("vtt-cache"))
                .unwrap_or_else(|| std::path::PathBuf::from("vtt-cache"));
            let _ = std::fs::create_dir_all(&dir);
            dir
        },
        tls: cfg.tls.as_ref().map(|t| http::TlsInfo {
            hostname: t.hostname.clone(),
            port: t.bind.port(),
            redirect_pages: t.redirect_pages,
        }),
        subs_inflight: Default::default(),
    });

    // Bump SystemUpdateID whenever the scanner commits, so browsing clients
    // know to refresh. data_version changes on any other-connection commit.
    spawn_db_watch(&db_path, state.clone())?;

    let interfaces = cfg.ssdp_interfaces();
    tracing::info!(
        "SSDP announcing on [{}], canonical address {ip}",
        interfaces
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let ssdp_socket = ssdp::bind_socket(&interfaces)?;
    let ssdp_senders = ssdp::make_senders(&interfaces)?;
    tokio::spawn(ssdp::respond_loop(
        ssdp_socket.clone(),
        uuid.clone(),
        location.clone(),
    ));
    tokio::spawn(ssdp::alive_loop(
        ssdp_senders.clone(),
        cfg.ssdp_unicast_clients.clone(),
        uuid.clone(),
        location.clone(),
        cfg.ssdp_alive_secs,
    ));

    let listener = tokio::net::TcpListener::bind(cfg.bind)
        .await
        .with_context(|| format!("binding {}", cfg.bind))?;
    tracing::info!("\"{}\" serving at {base_url} (uuid {uuid})", cfg.friendly_name);

    // Optional HTTPS listener for the web pages: same router, second
    // port, certificate re-read periodically so renewals need no restart.
    let tls_handle = match &cfg.tls {
        Some(tls) => Some(spawn_tls_listener(tls, http::router_tls(state.clone())).await?),
        None => None,
    };

    axum::serve(listener, http::router(state))
        .with_graceful_shutdown(async move {
            // SIGINT for the terminal, SIGTERM for systemd stop/restart.
            #[cfg(unix)]
            {
                let mut term = tokio::signal::unix::signal(
                    tokio::signal::unix::SignalKind::terminate(),
                )
                .expect("installing SIGTERM handler");
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            #[cfg(not(unix))]
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down: sending ssdp:byebye");
        })
        .await?;

    if let Some(handle) = tls_handle {
        handle.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
    }
    ssdp::byebye(&ssdp_senders, &cfg.ssdp_unicast_clients, &uuid, &location).await;
    Ok(())
}

/// Bind the HTTPS port (failing loudly now rather than in a background
/// task), serve `app` on it, and keep the certificate fresh.
async fn spawn_tls_listener(
    tls: &config::TlsConfig,
    app: axum::Router,
) -> Result<axum_server::Handle> {
    use axum_server::tls_rustls::RustlsConfig;
    let rustls = RustlsConfig::from_pem_file(&tls.cert, &tls.key)
        .await
        .with_context(|| format!("loading TLS certificate {} / key {}", tls.cert.display(), tls.key.display()))?;
    let std_listener = std::net::TcpListener::bind(tls.bind)
        .with_context(|| format!("binding {} for HTTPS", tls.bind))?;
    std_listener.set_nonblocking(true)?;
    let handle = axum_server::Handle::new();
    let origin = http::TlsInfo {
        hostname: tls.hostname.clone(),
        port: tls.bind.port(),
        redirect_pages: tls.redirect_pages,
    }
    .origin();
    tracing::info!(
        "web pages also at {origin} (HTTPS on {}{})",
        tls.bind,
        if tls.redirect_pages { "; plain-port page requests redirect there" } else { "" }
    );

    let (cert, key, every) = (tls.cert.clone(), tls.key.clone(), tls.reload_secs.max(30));
    let reload = rustls.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(every)).await;
            match reload.reload_from_pem_file(&cert, &key).await {
                Ok(()) => tracing::debug!("TLS certificate re-read from {}", cert.display()),
                Err(err) => tracing::warn!("TLS certificate reload failed ({}): {err}", cert.display()),
            }
        }
    });

    let server_handle = handle.clone();
    tokio::spawn(async move {
        let result = axum_server::from_tcp_rustls(std_listener, rustls)
            .handle(server_handle)
            .serve(app.into_make_service())
            .await;
        if let Err(err) = result {
            tracing::error!("HTTPS listener stopped: {err}");
        }
    });
    Ok(handle)
}

fn spawn_db_watch(db_path: &std::path::Path, state: Arc<AppState>) -> Result<()> {
    let conn = media_db::open_ro(db_path)?;
    std::thread::spawn(move || {
        let mut last: i64 = -1;
        loop {
            match conn.query_row("PRAGMA data_version", [], |r| r.get::<_, i64>(0)) {
                Ok(version) => {
                    if last >= 0 && version != last {
                        let id = state.update_id.fetch_add(1, Ordering::Relaxed) + 1;
                        tracing::info!("catalog changed; SystemUpdateID -> {id}");
                    }
                    last = version;
                }
                Err(err) => tracing::warn!("data_version poll failed: {err}"),
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
    Ok(())
}
