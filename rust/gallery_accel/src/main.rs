use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use gallery_accel::upstream::Upstream;
use gallery_accel::{env_db_path, log_error, log_info, spawn_configured_workers};

mod route_params;
mod routes;

#[derive(Parser, Debug)]
struct Args {
    #[arg(long)]
    db: Option<PathBuf>,
    /// Bind address. Defaults to 127.0.0.1, or 0.0.0.0 in primary mode. An
    /// explicit value is always honoured, so a test can stay loopback-only.
    #[arg(long)]
    host: Option<String>,
    /// Bind port. Defaults to 18899, or 8899 in primary mode. An explicit
    /// value is always honoured.
    #[arg(long)]
    port: Option<u16>,
    #[arg(long, default_value_t = 16)]
    pool_size: usize,
    /// Open the database read-only (default). Implied off when writes are enabled.
    #[arg(long, default_value_t = true)]
    read_only: bool,
    /// Enable write API routes. Forces the database open read-write.
    #[arg(long, default_value_t = false)]
    enable_writes: bool,
    /// Enable media (file serve / stream / transcode) routes.
    #[arg(long, default_value_t = false)]
    enable_media: bool,
    /// Enable ML inference routes.
    #[arg(long, default_value_t = false)]
    enable_ml: bool,
    /// Run as the public product process on the service port (default host/port
    /// become 0.0.0.0:8899 when not overridden). Serves static UI and proxies
    /// residual domains to `--upstream`.
    #[arg(long, default_value_t = false)]
    primary: bool,
    /// Residual Python FastAPI base URL (e.g. http://127.0.0.1:18900).
    #[arg(long)]
    upstream: Option<String>,
    /// Directory containing index.html / style.css / js for primary mode.
    #[arg(long)]
    static_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = Args::parse();
    if args.primary {
        // Capability defaults only: an explicit --host/--port is never
        // rewritten, otherwise a primary-mode smoke test could not restrict
        // itself to loopback.
        args.enable_writes = true;
        args.enable_media = true;
        args.read_only = false;
    }

    let host = args.host.clone().unwrap_or_else(|| {
        if args.primary {
            "0.0.0.0".to_string()
        } else {
            "127.0.0.1".to_string()
        }
    });
    let port = args.port.unwrap_or_else(|| {
        if args.primary {
            std::env::var("TRIM_SERVICE_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .or_else(|| std::env::var("PORT").ok().and_then(|v| v.parse().ok()))
                .unwrap_or(8899)
        } else {
            18899
        }
    });

    let db_path = args.db.unwrap_or_else(env_db_path);
    let read_only = args.read_only && !args.enable_writes;
    let upstream = match args.upstream.as_deref() {
        Some(url) if !url.trim().is_empty() => Some(Upstream::new(url)?),
        _ => None,
    };
    let state = routes::AppState::with_options(
        db_path.clone(),
        gallery_accel::DbConfig {
            read_only,
            pool_size: args.pool_size,
        },
        routes::Capabilities {
            read_only,
            writes: args.enable_writes,
            media: args.enable_media,
            ml: args.enable_ml,
        },
        upstream,
        args.primary,
    )?;

    // Finish or discard deletes interrupted by crash/power loss before any
    // request can observe the half-state ('moving' recycle rows).
    if !read_only {
        let cleaned_spools = gallery_accel::scan::cleanup_stale_presence_spools(
            gallery_accel::scan::PRESENCE_SPOOL_MAX_AGE,
        );
        if cleaned_spools > 0 {
            log_info!("cleaned {cleaned_spools} stale presence spool files on startup");
        }
        // Deliberately a dedicated short-lived connection, not the pool:
        // this must run before the listener binds and before AppState (and
        // its DbPool) exist. Keep journal_mode/busy_timeout aligned with
        // configure_connection so pool connections never meet a
        // differently-configured peer on the same WAL file.
        match rusqlite::Connection::open(&db_path) {
            Ok(conn) => {
                let _ = conn.execute_batch(&format!(
                    "PRAGMA busy_timeout={}; PRAGMA journal_mode=WAL;",
                    gallery_accel::DEFAULT_SQLITE_BUSY_TIMEOUT_MS
                ));
                match gallery_accel::ensure_recycle_schema(&conn) {
                    Ok(()) => {
                        let (finalized, dropped, missing) =
                            gallery_accel::reconcile_moving_recycle_entries(&conn);
                        if finalized + dropped + missing > 0 {
                            log_info!(
                                "recycle reconciliation: finalized={finalized} dropped={dropped} marked_missing={missing}"
                            );
                        }
                    }
                    Err(error) => log_error!("recycle schema check failed: {error}"),
                }
                let move_reconciliation = gallery_accel::reconcile_pending_artist_move(&conn);
                if move_reconciliation["reconciled"] == serde_json::json!(true) {
                    log_info!(
                        "artist move reconciliation: {}",
                        move_reconciliation["outcome"].as_str().unwrap_or("unknown")
                    );
                }
                // A scan marker left behind by a killed process would disable
                // the panel's scan button and every idle tick that checks it.
                match gallery_accel::reconcile_interrupted_scan(&conn) {
                    Ok(1) => log_info!("cleared an interrupted scan marker from a previous run"),
                    Ok(_) => {}
                    Err(error) => log_error!("scan state reconciliation failed: {error}"),
                }
                // A publish interrupted between creating its staging file and
                // recording it leaves a file the size of the finished download
                // that nothing else can find: the scanner skips dot-files and
                // the ledger never learned the path.
                match gallery_accel::ingest_publish::sweep_orphaned_publish_parts(
                    &conn,
                    &gallery_accel::env_media_roots(),
                    gallery_accel::ingest_publish::ORPHANED_PART_MIN_AGE,
                ) {
                    Ok(sweep) if sweep.files > 0 => log_info!(
                        "reclaimed {} orphaned publish staging files ({} bytes)",
                        sweep.files,
                        sweep.bytes
                    ),
                    Ok(_) => {}
                    Err(error) => log_error!("orphaned publish staging sweep failed: {error}"),
                }
            }
            Err(error) => log_error!("recycle reconciliation open failed: {error}"),
        }
    }

    // Start background runtime preparation only for an ML-enabled process.
    // The worker opens SQLite itself so a busy database cannot delay listener
    // readiness.
    if args.enable_ml {
        let _ = gallery_accel::runtime_prepare::prepare_runtime_at(&db_path);
    }

    // Optional character idle import (CHARACTER_IMPORT_IDLE_ENABLED=1 only).
    //
    // The scan control is cloned out of the worker inputs so the shutdown path
    // can tell every worker loop that the process is leaving, whether or not
    // the workers were started in this process.
    let mut shutdown_control: Option<Arc<gallery_accel::ScanControl>> = None;
    if args.primary && !read_only {
        let (worker_pool, worker_roots, worker_scan, worker_status, stats_gate) =
            state.worker_inputs();
        shutdown_control = Some(Arc::clone(&worker_scan));
        spawn_configured_workers(
            worker_pool,
            worker_roots,
            worker_scan,
            worker_status,
            stats_gate,
        );
        if let Ok(idle_pool) = gallery_accel::DbPool::with_config(
            db_path.clone(),
            gallery_accel::DbConfig {
                read_only: false,
                pool_size: 1,
            },
        ) {
            gallery_accel::spawn_character_import_idle_worker(std::sync::Arc::new(idle_pool));
        }
    }

    let mut app = routes::router(state);
    if args.primary {
        let static_dir = args.static_dir.unwrap_or_else(default_static_dir);
        app = routes::with_static_ui(app, static_dir);
    }

    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    log_info!(
        "gallery_accel listening on http://{} primary={} writes={}",
        addr,
        args.primary,
        args.enable_writes
    );
    // The peer address is published to the handlers, so a route that is only
    // meant for a local companion (the downloader bridge) can say so.
    //
    // Shutdown: the package is stopped with SIGTERM and killed one second
    // later, so the default disposition would cut whatever is running. The
    // signal now ends the accept loop, tells the workers to stop between units
    // of work, and gives the in-flight work a short window to finish. What is
    // still running after that window is cut anyway — this narrows the window
    // the startup reconciliation has to repair, it does not remove it.
    let serve = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    );
    tokio::select! {
        result = serve => {
            result?;
        }
        _ = shutdown_signal() => {
            log_info!("shutdown requested; finishing in-flight work");
            if let Some(control) = shutdown_control.as_ref() {
                control.begin_shutdown();
            }
            tokio::time::sleep(SHUTDOWN_GRACE).await;
            log_info!("shutdown grace period elapsed");
        }
    }
    Ok(())
}

/// How long in-flight work may run after a stop request before the process
/// exits. fnOS kills five seconds after SIGTERM is ignored, so this stays well
/// inside that window and only has to cover writing terminal state.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// Resolve on SIGTERM (Unix) or Ctrl+C (everywhere).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(term) => term,
            Err(error) => {
                log_error!("cannot install the SIGTERM handler: {error}");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn default_static_dir() -> PathBuf {
    if let Ok(path) = std::env::var("GALLERY_STATIC_DIR") {
        let p = PathBuf::from(path);
        if p.is_dir() {
            return p;
        }
    }
    // FPK layout: $APP_DIR/app/static ; dev layout: repo/app/static
    for candidate in [
        PathBuf::from("app/static"),
        PathBuf::from("static"),
        PathBuf::from("/app/app/static"),
    ] {
        if candidate.is_dir() {
            return candidate;
        }
    }
    PathBuf::from("app/static")
}
