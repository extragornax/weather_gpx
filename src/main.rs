mod gpx_parse;
mod handlers;
mod weather;
mod wind;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    response::Html,
    routing::{get, post},
};
use tower_http::trace::TraceLayer;

use crate::handlers::AppState;
use crate::weather::WeatherCache;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,meteo_gpx=info")),
        )
        .init();

    let db_path = std::env::var("DB_PATH").unwrap_or_else(|_| "data/meteo.db".into());
    if let Some(parent) = PathBuf::from(&db_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let cache = Arc::new(WeatherCache::open(&db_path)?);

    let state = AppState { cache };

    let meteo = Router::new()
        .route("/", get(handlers::index))
        .route("/static/app.css", get(handlers::app_css))
        .route("/api/analyze", post(handlers::analyze))
        .with_state(state);

    let app = Router::new()
        .route("/", get(|| async {
            Html(r#"<!doctype html><html><head><meta charset="utf-8">
<meta http-equiv="refresh" content="4;url=https://gpx.extragornax.fr/meteo">
<style>body{font-family:system-ui,sans-serif;display:flex;justify-content:center;align-items:center;min-height:100vh;margin:0;background:#f2e9d4;color:#0a1f3b}
.box{text-align:center;max-width:480px;padding:2rem}.box h1{font-size:1.4rem;margin:0 0 .8rem}.box p{margin:0 0 .6rem;line-height:1.5}
a{color:#b8242a}</style></head><body><div class="box">
<h1>This service has moved</h1>
<p>Meteo Dispatch now lives at<br><a href="https://gpx.extragornax.fr/meteo">gpx.extragornax.fr/meteo</a></p>
<p style="font-size:.85rem;opacity:.7">Redirecting in a few seconds…</p>
</div></body></html>"#)
        }))
        .nest("/meteo", meteo)
        .layer(DefaultBodyLimit::max(20 * 1024 * 1024))
        .layer(TraceLayer::new_for_http());

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3000);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "meteo_gpx listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut sig) = signal(SignalKind::terminate()) {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown");
}
