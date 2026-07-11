use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use gallery_accel::upstream::Upstream;
use gallery_accel::{env_db_path, spawn_configured_workers};

mod route_params;
mod routes;
#[cfg(test)]
mod test_support;

#[derive(Parser, Debug)]
struct Args {
    #[arg(long)]
    db: Option<PathBuf>,
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 18899)]
    port: u16,
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
        if args.host == "127.0.0.1" {
            args.host = "0.0.0.0".to_string();
        }
        if args.port == 18899 {
            args.port = std::env::var("TRIM_SERVICE_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .or_else(|| std::env::var("PORT").ok().and_then(|v| v.parse().ok()))
                .unwrap_or(8899);
        }
        args.enable_writes = true;
        args.enable_media = true;
        args.read_only = false;
    }

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

    // Optional character idle import (CHARACTER_IMPORT_IDLE_ENABLED=1 only).
    if args.primary && !read_only {
        let (worker_pool, worker_roots, worker_scan, worker_status) = state.worker_inputs();
        spawn_configured_workers(worker_pool, worker_roots, worker_scan, worker_status);
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

    let addr: SocketAddr = format!("{}:{}", args.host, args.port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!(
        "gallery_accel listening on http://{} primary={} writes={}",
        addr, args.primary, args.enable_writes
    );
    axum::serve(listener, app).await?;
    Ok(())
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
