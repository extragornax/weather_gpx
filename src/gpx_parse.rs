use std::io::BufReader;

use anyhow::{Context, Result, bail};
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct RawPoint {
    pub lat: f64,
    pub lon: f64,
    pub ele: Option<f64>,
}

/// A sample taken at a regular km interval along the route.
/// `bearing_deg` is the compass heading (0 = north, 90 = east) from this
/// point toward the next sample — i.e. the direction the rider is going.
#[derive(Debug, Clone, Serialize)]
pub struct KmSample {
    pub km: f64,
    pub lat: f64,
    pub lon: f64,
    pub ele: f64,
    pub bearing_deg: f64,
}

pub fn parse_track(reader: impl std::io::Read) -> Result<Vec<RawPoint>> {
    let g = gpx::read(BufReader::new(reader)).context("failed to parse GPX")?;
    let mut pts = Vec::new();
    for track in g.tracks {
        for segment in track.segments {
            for p in segment.points {
                let pt = p.point();
                pts.push(RawPoint {
                    lat: pt.y(),
                    lon: pt.x(),
                    ele: p.elevation,
                });
            }
        }
    }
    // Fall back to route points if there's no track (some GPX from planners).
    if pts.is_empty() {
        for route in g.routes {
            for p in route.points {
                let pt = p.point();
                pts.push(RawPoint {
                    lat: pt.y(),
                    lon: pt.x(),
                    ele: p.elevation,
                });
            }
        }
    }
    if pts.len() < 2 {
        bail!("GPX has no usable track points");
    }
    Ok(pts)
}

pub fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6371.0_f64;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    r * c
}

/// Initial bearing from (lat1, lon1) to (lat2, lon2) in degrees, 0..360.
pub fn bearing_deg(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let phi1 = lat1.to_radians();
    let phi2 = lat2.to_radians();
    let dl = (lon2 - lon1).to_radians();
    let y = dl.sin() * phi2.cos();
    let x = phi1.cos() * phi2.sin() - phi1.sin() * phi2.cos() * dl.cos();
    let b = y.atan2(x).to_degrees();
    (b + 360.0) % 360.0
}

/// Walk the raw track and return one `KmSample` every `step_km`, interpolating
/// lat/lon/ele when the step falls between two GPX points. The final point of
/// the route is always included.
pub fn sample_by_km(raw: &[RawPoint], step_km: f64) -> Vec<KmSample> {
    let mut cum = Vec::with_capacity(raw.len());
    cum.push(0.0_f64);
    for i in 1..raw.len() {
        let d = haversine_km(raw[i - 1].lat, raw[i - 1].lon, raw[i].lat, raw[i].lon);
        cum.push(cum[i - 1] + d);
    }
    let total = *cum.last().unwrap_or(&0.0);

    let mut samples = Vec::new();
    let mut target = 0.0;
    let mut seg = 1;

    while target <= total + 1e-9 {
        while seg < cum.len() && cum[seg] < target {
            seg += 1;
        }
        let (lat, lon, ele) = if seg >= cum.len() {
            let last = raw.last().unwrap();
            (last.lat, last.lon, last.ele.unwrap_or(0.0))
        } else {
            let a = &raw[seg - 1];
            let b = &raw[seg];
            let span = (cum[seg] - cum[seg - 1]).max(1e-9);
            let t = ((target - cum[seg - 1]) / span).clamp(0.0, 1.0);
            let lat = a.lat + (b.lat - a.lat) * t;
            let lon = a.lon + (b.lon - a.lon) * t;
            let ae = a.ele.unwrap_or(0.0);
            let be = b.ele.unwrap_or(ae);
            let ele = ae + (be - ae) * t;
            (lat, lon, ele)
        };
        samples.push(KmSample {
            km: target,
            lat,
            lon,
            ele,
            bearing_deg: 0.0,
        });
        target += step_km;
    }

    // Always include the final point.
    if let Some(last_sample) = samples.last()
        && (total - last_sample.km).abs() > 1e-6
    {
        let last = raw.last().unwrap();
        samples.push(KmSample {
            km: total,
            lat: last.lat,
            lon: last.lon,
            ele: last.ele.unwrap_or(0.0),
            bearing_deg: 0.0,
        });
    }

    // Fill in bearings from each sample to the next (last one copies previous).
    for i in 0..samples.len() {
        if i + 1 < samples.len() {
            samples[i].bearing_deg =
                bearing_deg(samples[i].lat, samples[i].lon, samples[i + 1].lat, samples[i + 1].lon);
        } else if i > 0 {
            samples[i].bearing_deg = samples[i - 1].bearing_deg;
        }
    }

    samples
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearing_north() {
        let b = bearing_deg(48.0, 2.0, 49.0, 2.0);
        assert!(b.abs() < 1.0 || (b - 360.0).abs() < 1.0);
    }

    #[test]
    fn bearing_east() {
        let b = bearing_deg(48.0, 2.0, 48.0, 3.0);
        assert!((b - 90.0).abs() < 1.0);
    }

    #[test]
    fn haversine_sanity() {
        let d = haversine_km(48.8566, 2.3522, 51.5074, -0.1278);
        assert!(d > 300.0 && d < 400.0);
    }
}
