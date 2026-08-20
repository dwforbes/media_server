mod config;
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
        uuid: uuid.clone(),
        friendly_name: cfg.friendly_name.clone(),
        base_url: base_url.clone(),
        icon,
        recent_count: cfg.recent_count,
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

    ssdp::byebye(&ssdp_senders, &cfg.ssdp_unicast_clients, &uuid, &location).await;
    Ok(())
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
