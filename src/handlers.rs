use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    Json,
    extract::{Path as AxumPath, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::gpx_parse::{KmSample, parse_track, sample_by_km};
use crate::weather::WeatherCache;
use crate::wind::{RidePoint, WindowScore, headwind_exposure, ride_forecast};

#[derive(Clone)]
pub struct AppState {
    pub cache: Arc<WeatherCache>,
    pub samples_dir: PathBuf,
}

pub async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

#[derive(Serialize)]
pub struct SampleEntry {
    pub name: String,
    pub path: String,
}

pub async fn list_samples(State(s): State<AppState>) -> Json<Vec<SampleEntry>> {
    Json(collect_samples(&s.samples_dir))
}

fn collect_samples(root: &Path) -> Vec<SampleEntry> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let Ok(dir) = std::fs::read_dir(root.join(&rel)) else {
            continue;
        };
        for entry in dir.flatten() {
            let Ok(name) = entry.file_name().into_string() else { continue };
            if name.starts_with('.') {
                continue;
            }
            let rel_path = rel.join(&name);
            let abs = root.join(&rel_path);
            if abs.is_dir() {
                stack.push(rel_path);
            } else if name.to_lowercase().ends_with(".gpx") {
                out.push(SampleEntry {
                    name: rel_path.to_string_lossy().to_string(),
                    path: rel_path.to_string_lossy().to_string(),
                });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub async fn get_sample(
    State(s): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Response {
    if name.contains("..") || name.starts_with('/') {
        return (StatusCode::BAD_REQUEST, "bad path").into_response();
    }
    let p = s.samples_dir.join(&name);
    match std::fs::read(&p) {
        Ok(bytes) => ([(header::CONTENT_TYPE, "application/gpx+xml")], bytes).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

#[derive(Deserialize)]
pub struct AnalyzeReq {
    /// Raw GPX XML (either pasted or fetched from a sample).
    pub gpx: String,
    /// RFC3339 timestamp for departure. If omitted, "now" is used.
    #[serde(default)]
    pub start: Option<String>,
    /// Average speed in km/h. Defaults to 25.
    #[serde(default)]
    pub speed_kmh: Option<f64>,
    /// If set, skip the departure-window sweep (saves time on long rides).
    #[serde(default)]
    pub skip_window: bool,
}

#[derive(Serialize)]
pub struct AnalyzeResp {
    pub total_km: f64,
    pub start_unix: i64,
    pub speed_kmh: f64,
    pub samples: Vec<RidePoint>,
    pub window: Vec<WindowScore>,
    pub best_window_start_unix: Option<i64>,
}

pub async fn analyze(
    State(state): State<AppState>,
    Json(req): Json<AnalyzeReq>,
) -> Result<Json<AnalyzeResp>, (StatusCode, String)> {
    let raw = parse_track(std::io::Cursor::new(req.gpx.as_bytes()))
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("gpx: {e}")))?;

    // ~1 km sampling: good resolution on the strip band without blowing up
    // the open-meteo cell count on long rides.
    let samples = sample_by_km(&raw, 1.0);
    if samples.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "no samples".into()));
    }
    let total_km = samples.last().map(|s| s.km).unwrap_or(0.0);

    let start = match &req.start {
        Some(s) => DateTime::parse_from_rfc3339(s)
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("start: {e}")))?
            .with_timezone(&Utc),
        None => Utc::now(),
    };
    let speed = req.speed_kmh.unwrap_or(25.0).max(5.0);

    // Collect every (lat, lon, hour) we need to look up, including the
    // 48h x 30min window so we only warm the cache once.
    let unique_cells = unique_cells(&samples);
    let mut req_list: Vec<(f64, f64, DateTime<Utc>)> = Vec::new();
    let window_start = start;
    let window_offsets_min: Vec<i64> = if req.skip_window {
        vec![0]
    } else {
        (0..=(48 * 60)).step_by(30).map(|m| m as i64).collect()
    };
    for off in &window_offsets_min {
        let t0 = window_start + chrono::Duration::minutes(*off);
        for s in &samples {
            let eta = t0 + chrono::Duration::seconds(((s.km / speed) * 3600.0) as i64);
            req_list.push((s.lat, s.lon, eta));
        }
    }
    // Collapse to a unique set (cell, hour) to avoid redundant work inside
    // the cache. The cache itself is already grid-keyed, so passing duplicates
    // is cheap but wasteful on Vec size.
    req_list.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.2.timestamp().cmp(&b.2.timestamp()))
    });

    // We only need the cache to be warm for each unique cell — the cache will
    // fetch a 7-day hourly block per cell, so we don't need to re-request for
    // every offset. Warm once:
    let warm: Vec<(f64, f64, DateTime<Utc>)> = unique_cells
        .iter()
        .map(|(lat, lon)| (*lat, *lon, start))
        .collect();
    let _ = state
        .cache
        .forecasts_for(&warm)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("weather: {e}")))?;

    // Selected-departure forecast.
    let picked: Vec<(f64, f64, DateTime<Utc>)> = samples
        .iter()
        .map(|s| {
            let eta = start + chrono::Duration::seconds(((s.km / speed) * 3600.0) as i64);
            (s.lat, s.lon, eta)
        })
        .collect();
    let picked_fc = state
        .cache
        .forecasts_for(&picked)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("weather: {e}")))?;
    let ride_points = ride_forecast(&samples, &picked_fc, start, speed);

    // Departure-window sweep.
    let mut window = Vec::new();
    let mut best: Option<(i64, f64)> = None;
    if !req.skip_window {
        for off in &window_offsets_min {
            let t0 = start + chrono::Duration::minutes(*off);
            let reqs: Vec<(f64, f64, DateTime<Utc>)> = samples
                .iter()
                .map(|s| {
                    (
                        s.lat,
                        s.lon,
                        t0 + chrono::Duration::seconds(((s.km / speed) * 3600.0) as i64),
                    )
                })
                .collect();
            let hp = state
                .cache
                .forecasts_for(&reqs)
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, format!("weather: {e}")))?;
            let ex = headwind_exposure(&samples, &hp, speed);
            window.push(WindowScore {
                start_unix: t0.timestamp(),
                exposure: ex,
            });
            match best {
                Some((_, e)) if e <= ex => {}
                _ => best = Some((t0.timestamp(), ex)),
            }
        }
    }

    Ok(Json(AnalyzeResp {
        total_km,
        start_unix: start.timestamp(),
        speed_kmh: speed,
        samples: ride_points,
        window,
        best_window_start_unix: best.map(|(t, _)| t),
    }))
}

fn unique_cells(samples: &[KmSample]) -> Vec<(f64, f64)> {
    // 0.1° rounding mirrors the cache grid.
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for s in samples {
        let key = (
            (s.lat * 10.0).round() as i64,
            (s.lon * 10.0).round() as i64,
        );
        if seen.insert(key) {
            out.push((s.lat, s.lon));
        }
    }
    out
}

const INDEX_HTML: &str = include_str!("../static/index.html");
