//! Drive list and formatting shared by the terminal and desktop dashboards.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::poll;
use crate::score::Tier;
use crate::store::{Snapshot, Store};
use crate::Reading;

/// History points kept per drive for the trend.
pub const HISTORY_POINTS: i64 = 32;

/// Points drawn in a compact trend.
pub const TREND_POINTS: usize = 12;

pub struct DiskView {
    pub reading: Reading,
    pub history: Vec<Snapshot>,
}

impl DiskView {
    /// Stored scores, oldest first.
    pub fn scores(&self) -> Vec<f64> {
        self.history.iter().filter_map(|snapshot| snapshot.score).collect()
    }
}

/// Polls every drive and loads its history, most at risk first.
pub fn refresh(store: &mut Store) -> Result<Vec<DiskView>, String> {
    let readings = poll(store)?.readings;
    let mut views = Vec::with_capacity(readings.len());
    for reading in readings {
        let history =
            store.history(reading.disk_id, HISTORY_POINTS).map_err(|e| e.to_string())?;
        views.push(DiskView { reading, history });
    }
    sort_views(&mut views);
    Ok(views)
}

/// Healthy drives last, then the lowest score first.
pub fn sort_views(disks: &mut [DiskView]) {
    disks.sort_by(|a, b| {
        severity(b.reading.assessment.tier)
            .cmp(&severity(a.reading.assessment.tier))
            .then_with(|| {
                let left = a.reading.assessment.score.unwrap_or(f64::MAX);
                let right = b.reading.assessment.score.unwrap_or(f64::MAX);
                left.total_cmp(&right)
            })
    });
}

pub fn severity(tier: Tier) -> u8 {
    match tier {
        Tier::Critical => 4,
        Tier::Warning => 3,
        Tier::Watch => 2,
        Tier::Unknown => 1,
        Tier::Healthy => 0,
    }
}

pub fn score_text(score: Option<f64>) -> String {
    match score {
        Some(score) => format!("{:.0}%", score.floor()),
        None => "n/a".to_string(),
    }
}

pub fn temperature_text(temperature: Option<i64>) -> String {
    match temperature {
        Some(temperature) => format!("{temperature}C"),
        None => "-".to_string(),
    }
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

pub fn ago_text(seconds: i64) -> String {
    if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 60 * 60 {
        format!("{}m ago", seconds / 60)
    } else {
        format!("{}h ago", seconds / (60 * 60))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::score::Assessment;
    use crate::smart::{Disk, Kind};
    use std::collections::BTreeMap;

    fn sample(model: &str, tier: Tier, score: Option<f64>) -> DiskView {
        DiskView {
            reading: Reading {
                disk_id: 0,
                disk: Disk {
                    device: "/dev/test".to_string(),
                    serial: "SERIAL".to_string(),
                    model: model.to_string(),
                    firmware: None,
                    capacity_bytes: Some(1000204886016),
                    kind: Kind::Hdd,
                    temperature_c: Some(35),
                    smart_passed: Some(true),
                    attributes: BTreeMap::new(),
                    attribute_names: BTreeMap::new(),
                    attribute_values: BTreeMap::new(),
                    nvme: None,
                },
                assessment: Assessment {
                    score,
                    tier,
                    reasons: vec!["test reason".to_string()],
                },
            },
            history: Vec::new(),
        }
    }

    #[test]
    fn views_sort_by_risk_then_score() {
        let mut disks = vec![
            sample("healthy", Tier::Healthy, Some(100.0)),
            sample("critical", Tier::Critical, Some(9.0)),
            sample("warning", Tier::Warning, Some(10.0)),
            sample("watch", Tier::Watch, Some(50.0)),
        ];
        sort_views(&mut disks);
        let order: Vec<&str> =
            disks.iter().map(|view| view.reading.disk.model.as_str()).collect();
        assert_eq!(order, ["critical", "warning", "watch", "healthy"]);
    }

    #[test]
    fn human_bytes_uses_decimal_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(480103981056), "480.1 GB");
        assert_eq!(human_bytes(1000204886016), "1.0 TB");
    }

    #[test]
    fn scores_skip_samples_without_a_score() {
        let mut view = sample("disk", Tier::Unknown, None);
        view.history = vec![
            Snapshot { timestamp: 1, score: Some(100.0), temperature_c: None, worsening: false },
            Snapshot { timestamp: 2, score: None, temperature_c: None, worsening: false },
            Snapshot { timestamp: 3, score: Some(80.0), temperature_c: None, worsening: false },
        ];
        assert_eq!(view.scores(), vec![100.0, 80.0]);
    }
}
