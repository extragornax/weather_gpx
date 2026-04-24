use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::gpx_parse::KmSample;
use crate::weather::HourlyPoint;

/// Forecast at one km along the ride, with the wind component along the rider's
/// heading. `headwind_kmh` is positive for a headwind, negative for a tailwind.
#[derive(Debug, Clone, Serialize)]
pub struct RidePoint {
    pub km: f64,
    pub lat: f64,
    pub lon: f64,
    pub ele: f64,
    pub bearing_deg: f64,
    pub eta_unix: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub precip_prob: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wind_speed_kmh: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wind_dir_deg: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headwind_kmh: Option<f64>,
}

/// Wind projected along the rider's heading.
///
/// `wind_dir_from` is the meteorological direction the wind is coming from
/// (0 = north). `heading_to` is the direction the rider is going toward
/// (same convention). A positive result means headwind, negative means tailwind.
pub fn headwind_component(wind_speed: f64, wind_dir_from: f64, heading_to: f64) -> f64 {
    let delta = (wind_dir_from - heading_to).to_radians();
    wind_speed * delta.cos()
}

/// Build a per-km forecast along the route for a departure time and average speed.
pub fn ride_forecast(
    samples: &[KmSample],
    hourly: &[Option<HourlyPoint>],
    start: DateTime<Utc>,
    speed_kmh: f64,
) -> Vec<RidePoint> {
    samples
        .iter()
        .zip(hourly.iter())
        .map(|(s, hp)| {
            let eta = start + Duration::seconds(((s.km / speed_kmh) * 3600.0) as i64);
            let (temp, precip, ws, wd, hw) = match hp {
                Some(h) => (
                    Some(h.temperature_c),
                    Some(h.precip_prob),
                    Some(h.wind_speed_kmh),
                    Some(h.wind_dir_deg),
                    Some(headwind_component(
                        h.wind_speed_kmh,
                        h.wind_dir_deg,
                        s.bearing_deg,
                    )),
                ),
                None => (None, None, None, None, None),
            };
            RidePoint {
                km: s.km,
                lat: s.lat,
                lon: s.lon,
                ele: s.ele,
                bearing_deg: s.bearing_deg,
                eta_unix: eta.timestamp(),
                temperature_c: temp,
                precip_prob: precip,
                wind_speed_kmh: ws,
                wind_dir_deg: wd,
                headwind_kmh: hw,
            }
        })
        .collect()
}

/// Total headwind exposure in (km/h * h) — i.e. roughly "effective extra km
/// pedalled into the wind". Tailwind segments count as 0 (they don't hurt you,
/// they just help, and we're scoring pain, not net effort).
pub fn headwind_exposure(
    samples: &[KmSample],
    hourly: &[Option<HourlyPoint>],
    speed_kmh: f64,
) -> f64 {
    let mut total = 0.0;
    for i in 0..samples.len() {
        let Some(h) = hourly[i] else { continue };
        let hw = headwind_component(h.wind_speed_kmh, h.wind_dir_deg, samples[i].bearing_deg);
        if hw <= 0.0 {
            continue;
        }
        let prev_km = if i == 0 { 0.0 } else { samples[i - 1].km };
        let seg_km = (samples[i].km - prev_km).max(0.0);
        let hours = seg_km / speed_kmh;
        total += hw * hours;
    }
    total
}

#[derive(Debug, Clone, Serialize)]
pub struct WindowScore {
    pub start_unix: i64,
    pub exposure: f64,
}
