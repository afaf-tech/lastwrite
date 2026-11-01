//! Health score and risk tier. HDD rules match `backtest/backtest.py`.

use crate::smart::{Disk, Kind, NVME_MEDIA_ERRORS_ID};

/// Counters that count as damage: reallocated, spin retry, pending,
/// uncorrectable, NVMe media errors.
pub const CRITICAL_IDS: [u32; 5] = [5, 10, 197, 198, NVME_MEDIA_ERRORS_ID];

const PENDING_PENALTY: f64 = 5.0;
const SPINDLE_PENALTY: f64 = 10.0;
const REALLOC_TIERS: (f64, f64) = (10.0, 100.0);
const WARNING_BELOW: f64 = 20.0;
const FAILING_CAP: f64 = 9.0;
/// NVMe critical warning bits, leaving out the temperature bit (0x02), which
/// clears by itself and has its own alert.
const NVME_CRITICAL_BITS: u64 = 0xFF & !0x02;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Tier {
    Healthy,
    Watch,
    Warning,
    Critical,
    Unknown,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Tier::Healthy => "Healthy",
            Tier::Watch => "Watch",
            Tier::Warning => "Warning",
            Tier::Critical => "CRITICAL",
            Tier::Unknown => "Unknown",
        }
    }
}

#[derive(Debug)]
pub struct Assessment {
    pub score: Option<f64>,
    pub tier: Tier,
    pub reasons: Vec<String>,
}

pub fn assess(disk: &Disk, worsening: bool) -> Assessment {
    let mut assessment = match disk.kind {
        Kind::Hdd => assess_hdd(disk, worsening),
        Kind::Nvme => assess_nvme(disk, worsening),
        Kind::SataSsd | Kind::Unknown => unscored(disk),
    };
    if disk.smart_passed == Some(false) {
        assessment.score = Some(assessment.score.unwrap_or(FAILING_CAP).min(FAILING_CAP));
        assessment.tier = Tier::Critical;
        assessment.reasons.push("Drive reports SMART overall health FAILED".to_string());
    }
    assessment
}

fn attribute(disk: &Disk, id: u32) -> f64 {
    disk.attributes.get(&id).copied().unwrap_or(0).max(0) as f64
}

fn assess_hdd(disk: &Disk, worsening: bool) -> Assessment {
    let (reallocated, spindle) = (attribute(disk, 5), attribute(disk, 10));
    let (pending, uncorrectable) = (attribute(disk, 197), attribute(disk, 198));
    let (low, high) = REALLOC_TIERS;
    let realloc_penalty = reallocated.min(low)
        + (reallocated.min(high) - low).max(0.0) * 0.5
        + (reallocated - high).max(0.0) * 0.2;

    let mut penalty = 0.0;
    let mut reasons = Vec::new();
    let mut add = |points: f64, count: f64, what: &str| {
        if points > 0.0 {
            penalty += points;
            reasons.push(format!("{count:.0} {what} (-{points:.1})"));
        }
    };
    add(PENDING_PENALTY * pending, pending, "pending sectors");
    add(PENDING_PENALTY * uncorrectable, uncorrectable, "uncorrectable sectors");
    add(realloc_penalty, reallocated, "reallocated sectors");
    add(if spindle > 0.0 { SPINDLE_PENALTY } else { 0.0 }, spindle, "spin retries");

    let score = (100.0 - penalty).max(0.0);
    let tier = if worsening {
        Tier::Critical
    } else if score < WARNING_BELOW {
        Tier::Warning
    } else if score < 100.0 {
        Tier::Watch
    } else {
        Tier::Healthy
    };
    if worsening {
        reasons.insert(0, "Damage counters rose in the last 7 days".to_string());
    }
    Assessment { score: Some(score), tier, reasons }
}

fn assess_nvme(disk: &Disk, worsening: bool) -> Assessment {
    let Some(health) = &disk.nvme else {
        return unscored(disk);
    };
    let mut score = 100.0 - (health.percentage_used as f64).min(100.0);
    let mut tier = if score < WARNING_BELOW { Tier::Warning } else { Tier::Healthy };
    let mut reasons = vec![format!("{}% of rated endurance used", health.percentage_used)];
    if health.media_errors > 0 {
        if tier == Tier::Healthy {
            tier = Tier::Watch;
        }
        reasons.push(format!("{} media errors", health.media_errors));
    }
    if worsening {
        tier = Tier::Critical;
        reasons.insert(0, "Media errors rose in the last 7 days".to_string());
    }
    if health.critical_warning & NVME_CRITICAL_BITS != 0 {
        score = score.min(FAILING_CAP);
        tier = Tier::Critical;
        reasons.push(format!("Controller critical warning 0x{:02x}", health.critical_warning));
    }
    if health.available_spare < health.available_spare_threshold {
        score = score.min(FAILING_CAP);
        tier = Tier::Critical;
        reasons.push(format!(
            "Available spare {}% is below the {}% threshold",
            health.available_spare, health.available_spare_threshold
        ));
    }
    Assessment { score: Some(score), tier, reasons }
}

fn unscored(disk: &Disk) -> Assessment {
    let reason = match disk.kind {
        Kind::SataSsd => "SATA SSD wear attributes are not mapped yet",
        _ => "Drive type is not supported yet",
    };
    Assessment { score: None, tier: Tier::Unknown, reasons: vec![reason.to_string()] }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smart::{parse, NvmeHealth};
    use std::collections::BTreeMap;

    fn hdd(values: &[(u32, i64)]) -> Disk {
        Disk {
            device: String::new(),
            serial: "S".to_string(),
            model: "M".to_string(),
            firmware: None,
            capacity_bytes: None,
            kind: Kind::Hdd,
            temperature_c: None,
            smart_passed: Some(true),
            attributes: values.iter().copied().collect::<BTreeMap<_, _>>(),
            nvme: None,
        }
    }

    fn nvme(used: u64, spare: u64, threshold: u64) -> Disk {
        let mut disk = hdd(&[]);
        disk.kind = Kind::Nvme;
        disk.nvme = Some(NvmeHealth {
            percentage_used: used,
            available_spare: spare,
            available_spare_threshold: threshold,
            critical_warning: 0,
            media_errors: 0,
        });
        disk
    }

    #[test]
    fn clean_hdd_is_healthy() {
        let result = assess(&hdd(&[(5, 0), (197, 0)]), false);
        assert_eq!((result.score, result.tier), (Some(100.0), Tier::Healthy));
    }

    #[test]
    fn realloc_penalty_is_tiered_like_the_backtest() {
        let result = assess(&hdd(&[(5, 150)]), false);
        assert_eq!(result.score, Some(35.0)); // 10 + 45 + 10
        assert_eq!(result.tier, Tier::Watch);
    }

    #[test]
    fn pending_uncorrectable_and_spindle_stack() {
        let result = assess(&hdd(&[(197, 1), (198, 1), (10, 3)]), false);
        assert_eq!(result.score, Some(80.0));
    }

    #[test]
    fn low_score_is_warning_and_score_never_goes_negative() {
        let result = assess(&hdd(&[(197, 50)]), false);
        assert_eq!((result.score, result.tier), (Some(0.0), Tier::Warning));
    }

    #[test]
    fn worsening_is_critical_but_leaves_score_alone() {
        let result = assess(&hdd(&[(197, 1)]), true);
        assert_eq!((result.score, result.tier), (Some(95.0), Tier::Critical));
    }

    #[test]
    fn smart_failure_overrides_everything() {
        let mut disk = hdd(&[]);
        disk.smart_passed = Some(false);
        let result = assess(&disk, false);
        assert_eq!((result.score, result.tier), (Some(9.0), Tier::Critical));
    }

    #[test]
    fn nvme_score_is_remaining_endurance() {
        let result = assess(&nvme(3, 100, 99), false);
        assert_eq!((result.score, result.tier), (Some(97.0), Tier::Healthy));
    }

    #[test]
    fn nvme_spare_kill_switch_caps_score() {
        let result = assess(&nvme(3, 5, 10), false);
        assert_eq!((result.score, result.tier), (Some(9.0), Tier::Critical));
    }

    #[test]
    fn nvme_media_errors_are_not_healthy() {
        let mut disk = nvme(3, 100, 99);
        disk.nvme.as_mut().unwrap().media_errors = 2;
        assert_eq!(assess(&disk, false).tier, Tier::Watch);
        assert_eq!(assess(&disk, true).tier, Tier::Critical);
    }

    #[test]
    fn nvme_critical_warning_caps_score() {
        let mut disk = nvme(3, 100, 99);
        disk.nvme.as_mut().unwrap().critical_warning = 0x04;
        let result = assess(&disk, false);
        assert_eq!((result.score, result.tier), (Some(9.0), Tier::Critical));
    }

    #[test]
    fn nvme_temperature_warning_alone_is_not_critical() {
        let mut disk = nvme(3, 100, 99);
        disk.nvme.as_mut().unwrap().critical_warning = 0x02;
        assert_eq!(assess(&disk, false).tier, Tier::Healthy);
    }

    #[test]
    fn sata_ssd_is_unknown_not_healthy() {
        let disk = parse(r#"{"device":{"protocol":"ATA"},"serial_number":"X","rotation_rate":0}"#);
        let result = assess(&disk.unwrap(), false);
        assert_eq!((result.score, result.tier), (None, Tier::Unknown));
    }

    #[test]
    fn hdd_fixture_scores_from_real_shaped_json() {
        let disk = parse(include_str!("../tests/fixtures/hdd_synthetic.json")).unwrap();
        assert_eq!(assess(&disk, false).score, Some(79.0)); // 10 + 1 + 10
    }
}
