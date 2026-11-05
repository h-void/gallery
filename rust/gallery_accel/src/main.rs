use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

// Shared-surface note (parallel BootGate batch, 2026-10-24): the startup path
// below is the one place the boot-gate work and the pawchive work touch. The
// orphaned publish-staging sweep was moved out of `startup_initialization`
// into a detached thread after the router is published (see
// `run_deferred_publish_sweep`): on a large library the recursive directory
// walk costs minutes, and nothing in the serving path needs it first — a
// restart used to answer `initializing` for that whole window. If you need to
// reorder the startup sequence, change this note in the same commit.

use clap::Parser;
use gallery_accel::{env_db_path, log_error, log_info, spawn_configured_workers};

#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

mod route_params;
mod routes;

#[derive(Parser, Debug, Clone)]
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
    #[arg(long, default_value_t = 6)]
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
    /// become 0.0.0.0:8899 when not overridden). Serves static UI directly.
    #[arg(long, default_value_t = false)]
    primary: bool,
    /// Directory containing index.html / style.css / js for primary mode.
    #[arg(long)]
    static_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = Args::parse();
    if let Ok(pool_env) = std::env::var("DB_POOL_SIZE") {
        if let Ok(parsed) = pool_env.trim().parse::<usize>() {
            args.pool_size = parsed;
        }
    }
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

    let db_path = args.db.clone().unwrap_or_else(env_db_path);
    // Bind before anything heavy: the port has to answer from the first
    // moment, because "connection refused" during a post-update first boot
    // read as the app being down — and stayed that way for as long as the
    // initialization took, forever when it hung. The gate answers health with
    // the startup phase and every other route with `initializing` until the
    // real application is published.
    let gate = routes::BootGate::new();
    let static_dir = if args.primary {
        Some(args.static_dir.clone().unwrap_or_else(default_static_dir))
    } else {
        None
    };
    let app = routes::boot_gate_router(gate.clone(), static_dir);

    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    log_info!(
        "gallery_accel listening on http://{} primary={} writes={} (startup initialization follows; /api/health reports progress)",
        addr,
        args.primary,
        args.enable_writes
    );

    // The heavyweight startup, off the accept loop. Everything here is the
    // synchronous work that used to run before the bind: pool construction
    // (schema, WAL), crash reconciliations, workers. The phase label feeds
    // the gate's health answer and the watchdog below, so a future hang names
    // its step in the log instead of looking like a dead process.
    let shutdown_control_slot: Arc<std::sync::OnceLock<Arc<gallery_accel::ScanControl>>> =
        Arc::new(std::sync::OnceLock::new());
    let init_shutdown_slot = Arc::clone(&shutdown_control_slot);
    let init_gate = gate.clone();
    let init_args = args.clone();
    let init_db_path = db_path.clone();
    let init_handle = tokio::task::spawn_blocking(move || {
        startup_initialization(init_gate, init_args, init_db_path, init_shutdown_slot)
    });
    let init_abort_handle = init_handle.abort_handle();
    tokio::spawn(async move {
        if let Err(error) = init_handle.await {
            if error.is_panic() {
                log_error!("startup initialization task panicked; aborting: {error}");
                std::process::exit(101);
            }
        }
    });

    // A startup that outlasts a minute says so, with the phase it is in: the
    // difference between "hung on the GPU" and "still migrating" is exactly
    // what the log was missing while the port refused connections.
    {
        let watchdog_gate = gate.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            interval.tick().await;
            while !watchdog_gate.ready() {
                interval.tick().await;
                if watchdog_gate.ready() {
                    break;
                }
                log_info!(
                    "startup still initializing: {} ({}s elapsed)",
                    watchdog_gate.phase(),
                    watchdog_gate.elapsed_seconds()
                );
            }
        });
    }

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
            if let Some(control) = shutdown_control_slot.get() {
                control.begin_shutdown();
            }
            tokio::time::sleep(SHUTDOWN_GRACE).await;
            log_info!("shutdown grace period elapsed");
        }
    }
    init_abort_handle.abort();
    Ok(())
}

/// The pre-listener work of old, run behind the boot gate on the blocking
/// pool. Publishing the finished router into the gate slot is the "ready"
/// transition; until then the gate answers on its own.
fn startup_initialization(
    gate: routes::BootGate,
    args: Args,
    db_path: PathBuf,
    shutdown_control_slot: Arc<std::sync::OnceLock<Arc<gallery_accel::ScanControl>>>,
) {
    let read_only = args.read_only && !args.enable_writes;
    gate.set_phase("opening the database");
    let state = match routes::AppState::with_options(
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
        args.primary,
    ) {
        Ok(state) => state,
        Err(error) => {
            log_error!("startup failed while opening the database: {error}");
            std::process::exit(1);
        }
    };

    // Finish or discard deletes interrupted by crash/power loss before any
    // request can observe the half-state ('moving' recycle rows).
    if !read_only {
        gate.set_phase("reconciling interrupted operations");
        // Deliberately a dedicated short-lived connection, not the pool:
        // this must run before the shared pool serves requests and before
        // AppState (and its DbPool) are used. Keep journal_mode/busy_timeout
        // aligned with configure_connection so pool connections never meet a
        // differently-configured peer on the same WAL file.
        match rusqlite::Connection::open(&db_path) {
            Ok(conn) => {
                // `foreign_keys` is part of the alignment too, and it is the
                // one that matters most here: the reconciliation below deletes
                // `items` rows and relies on ON DELETE CASCADE / SET NULL for
                // item_tags, item_favorites, move_candidates, move_history,
                // link_index and character_references. With FKs off those rows
                // were left behind — and character_references stayed pointing at
                // a deleted item instead of becoming NULL, so a later restore's
                // reattach pass (which matches `item_id IS NULL`) found nothing.
                let _ = conn.execute_batch(&format!(
                    "PRAGMA busy_timeout={}; PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;",
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
                // The orphaned publish-staging sweep used to run here, but it
                // walks every directory under the authorized roots — minutes
                // of `read_dir` on a large library, with the whole serving
                // surface answering `initializing` meanwhile. It runs
                // detached after the router is published instead (see
                // `run_deferred_publish_sweep`).
            }
            Err(error) => log_error!("recycle reconciliation open failed: {error}"),
        }
    }

    // Start background runtime preparation only for an ML-enabled process.
    // The worker opens SQLite itself so a busy database cannot delay listener
    // readiness.
    if args.enable_ml {
        gate.set_phase("starting runtime preparation");
        let _ = gallery_accel::runtime_prepare::prepare_runtime_at(&db_path);
    }

    // Optional character idle import (CHARACTER_IMPORT_IDLE_ENABLED=1 only).
    //
    // The scan control is cloned out of the worker inputs so the shutdown path
    // can tell every worker loop that the process is leaving, whether or not
    // the workers were started in this process.
    if args.primary && !read_only {
        gate.set_phase("starting background workers");
        let (worker_pool, worker_roots, worker_scan, worker_status, stats_gate) =
            state.worker_inputs();
        let _ = shutdown_control_slot.set(Arc::clone(&worker_scan));
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
            gallery_accel::spawn_character_import_idle_worker(Arc::new(idle_pool));
        }
    }

    gate.set_phase("publishing the application");
    let real = routes::router(state);
    if let Err(previous) = gate.real_slot().set(real.into_service()) {
        // Only a double initialization can land here; the loser keeps quiet.
        let _ = previous;
    }
    log_info!(
        "startup complete in {}s: application published",
        gate.elapsed_seconds()
    );

    // The orphaned-staging sweep is a reclaim of `.gallery_publish_*.part`
    // files older than `ORPHANED_PART_MIN_AGE`, not a precondition of
    // serving: the scanner skips dot-files and every later boot retries it.
    // It walks every directory under the authorized roots — minutes on a
    // large library — so it runs detached here; a shutdown mid-sweep simply
    // leaves the work to the next boot.
    if !read_only {
        let sweep_db_path = db_path.clone();
        let spawned = std::thread::Builder::new()
            .name("publish-part-sweep".to_string())
            .spawn(move || run_deferred_publish_sweep(&sweep_db_path));
        if let Err(error) = spawned {
            log_error!("could not start the publish staging sweep thread: {error}");
        }
    }
}

/// The orphaned publish-staging sweep, detached from the startup path (see
/// the spawn site in `startup_initialization`). The connection's pragmas
/// mirror the startup reconciliation's so pool connections never meet a
/// differently-configured peer on the same WAL file.
fn run_deferred_publish_sweep(db_path: &std::path::Path) {
    let conn = match rusqlite::Connection::open(db_path) {
        Ok(conn) => conn,
        Err(error) => {
            log_error!("orphaned publish staging sweep open failed: {error}");
            return;
        }
    };
    let _ = conn.execute_batch(&format!(
        "PRAGMA busy_timeout={}; PRAGMA journal_mode=WAL;",
        gallery_accel::DEFAULT_SQLITE_BUSY_TIMEOUT_MS
    ));
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
