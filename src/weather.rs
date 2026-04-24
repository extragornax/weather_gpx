use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// How coarse the cache grid is. 0.1° ≈ 11km, which matches the effective
/// resolution of Open-Meteo's free tier and keeps the cache small.
const GRID_DEG: f64 = 0.1;
/// How long a fetched forecast is considered fresh. Open-Meteo updates their
/// forecast every few hours — 1h is a fair tradeoff between freshness and
/// hammering their servers while someone is scrubbing the departure slider.
const CACHE_TTL_SECS: i64 = 3600;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct HourlyPoint {
    pub temperature_c: f64,
    pub precip_prob: f64,
    pub wind_speed_kmh: f64,
    /// Direction the wind is COMING FROM, in meteorological degrees
    /// (0 = north, 90 = east).
    pub wind_dir_deg: f64,
}

pub struct WeatherCache {
    conn: Mutex<Connection>,
    http: reqwest::Client,
}

impl WeatherCache {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("open sqlite at {path}"))?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS forecast_cell (
                lat_cell INTEGER NOT NULL,
                lon_cell INTEGER NOT NULL,
                hour_unix INTEGER NOT NULL,
                temperature_c REAL NOT NULL,
                precip_prob REAL NOT NULL,
                wind_speed_kmh REAL NOT NULL,
                wind_dir_deg REAL NOT NULL,
                fetched_at INTEGER NOT NULL,
                PRIMARY KEY (lat_cell, lon_cell, hour_unix)
            );
            CREATE INDEX IF NOT EXISTS forecast_fetched
                ON forecast_cell (lat_cell, lon_cell, fetched_at);
            "#,
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
            http: reqwest::Client::builder()
                .user_agent("meteo_gpx/0.1 (+https://meteo.extragornax.fr)")
                .timeout(std::time::Duration::from_secs(15))
                .build()?,
        })
    }

    /// Fetch (or return cached) hourly forecasts for a set of (lat, lon, hour)
    /// lookups. Groups lookups by grid cell so we make at most one HTTP request
    /// per cell.
    pub async fn forecasts_for(
        &self,
        requests: &[(f64, f64, DateTime<Utc>)],
    ) -> Result<Vec<Option<HourlyPoint>>> {
        // Figure out which cells we need to fetch fresh.
        let mut needed: std::collections::HashMap<(i64, i64), (f64, f64)> = Default::default();
        {
            let conn = self.conn.lock().unwrap();
            for (lat, lon, _) in requests {
                let (la, lo) = grid_cell(*lat, *lon);
                if needed.contains_key(&(la, lo)) {
                    continue;
                }
                // Is any data for this cell still fresh?
                let fresh: Option<i64> = conn
                    .query_row(
                        "SELECT MAX(fetched_at) FROM forecast_cell WHERE lat_cell=? AND lon_cell=?",
                        params![la, lo],
                        |r| r.get::<_, Option<i64>>(0),
                    )
                    .optional()?
                    .flatten();
                let now = Utc::now().timestamp();
                if fresh.map(|f| now - f < CACHE_TTL_SECS).unwrap_or(false) {
                    continue;
                }
                needed.insert((la, lo), (*lat, *lon));
            }
        }

        for ((_, _), (lat, lon)) in &needed {
            if let Err(e) = self.fetch_cell(*lat, *lon).await {
                tracing::warn!(%lat, %lon, error=%e, "open-meteo fetch failed");
            }
        }

        // Resolve every request against the cache.
        let conn = self.conn.lock().unwrap();
        let mut out = Vec::with_capacity(requests.len());
        for (lat, lon, ts) in requests {
            let (la, lo) = grid_cell(*lat, *lon);
            let hour = ts.timestamp() - ts.timestamp().rem_euclid(3600);
            let row = conn
                .query_row(
                    "SELECT temperature_c, precip_prob, wind_speed_kmh, wind_dir_deg
                     FROM forecast_cell
                     WHERE lat_cell=? AND lon_cell=? AND hour_unix=?",
                    params![la, lo, hour],
                    |r| {
                        Ok(HourlyPoint {
                            temperature_c: r.get(0)?,
                            precip_prob: r.get(1)?,
                            wind_speed_kmh: r.get(2)?,
                            wind_dir_deg: r.get(3)?,
                        })
                    },
                )
                .optional()?;
            out.push(row);
        }
        Ok(out)
    }

    async fn fetch_cell(&self, lat: f64, lon: f64) -> Result<()> {
        let (la, lo) = grid_cell(lat, lon);
        // Snap to grid centre so every fetch for the same cell hits the same URL.
        let lat_q = la as f64 * GRID_DEG;
        let lon_q = lo as f64 * GRID_DEG;
        let url = format!(
            "https://api.open-meteo.com/v1/forecast?latitude={lat:.3}&longitude={lon:.3}&hourly=temperature_2m,precipitation_probability,wind_speed_10m,wind_direction_10m&forecast_days=7&timezone=UTC&wind_speed_unit=kmh",
            lat = lat_q,
            lon = lon_q,
        );
        let resp: OpenMeteoResp = self
            .http
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let times = resp.hourly.time;
        let temps = resp.hourly.temperature_2m;
        let precs = resp.hourly.precipitation_probability;
        let winds = resp.hourly.wind_speed_10m;
        let dirs = resp.hourly.wind_direction_10m;

        let now = Utc::now().timestamp();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        for i in 0..times.len() {
            let ts = match DateTime::parse_from_rfc3339(&format!("{}:00Z", times[i])) {
                Ok(t) => t.with_timezone(&Utc).timestamp(),
                Err(_) => continue,
            };
            // Open-Meteo can return null for precipitation_probability in some
            // regions; treat missing as 0.
            let precip = precs.get(i).copied().flatten().unwrap_or(0.0);
            let temp = match temps.get(i).copied().flatten() {
                Some(v) => v,
                None => continue,
            };
            let ws = match winds.get(i).copied().flatten() {
                Some(v) => v,
                None => continue,
            };
            let wd = match dirs.get(i).copied().flatten() {
                Some(v) => v,
                None => continue,
            };
            tx.execute(
                "INSERT OR REPLACE INTO forecast_cell
                 (lat_cell, lon_cell, hour_unix,
                  temperature_c, precip_prob, wind_speed_kmh, wind_dir_deg, fetched_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![la, lo, ts, temp, precip, ws, wd, now],
            )?;
        }
        tx.commit()?;
        tracing::info!(la, lo, "cached forecast cell");
        Ok(())
    }
}

fn grid_cell(lat: f64, lon: f64) -> (i64, i64) {
    ((lat / GRID_DEG).round() as i64, (lon / GRID_DEG).round() as i64)
}

#[derive(Deserialize)]
struct OpenMeteoResp {
    hourly: OpenMeteoHourly,
}

#[derive(Deserialize)]
struct OpenMeteoHourly {
    // ISO-8601 without seconds, e.g. "2026-04-24T13:00"
    time: Vec<String>,
    temperature_2m: Vec<Option<f64>>,
    precipitation_probability: Vec<Option<f64>>,
    wind_speed_10m: Vec<Option<f64>>,
    wind_direction_10m: Vec<Option<f64>>,
}
