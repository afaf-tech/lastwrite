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
        Kind::SataSsd => assess_sata_ssd(disk, worsening),
        Kind::Unknown => unscored(),
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

struct Damage {
    points: f64,
    reasons: Vec<String>,
}

impl Damage {
    fn add(&mut self, points: f64, count: f64, what: &str) {
        if points > 0.0 {
            self.points += points;
            self.reasons.push(format!("{count:.0} {what} (-{points:.1})"));
        }
    }
}

/// Sector damage is penalised the same way for HDDs and SATA SSDs.
fn sector_damage(disk: &Disk) -> Damage {
    let reallocated = attribute(disk, 5);
    let (pending, uncorrectable) = (attribute(disk, 197), attribute(disk, 198));
    let (low, high) = REALLOC_TIERS;
    let realloc_penalty = reallocated.min(low)
        + (reallocated.min(high) - low).max(0.0) * 0.5
        + (reallocated - high).max(0.0) * 0.2;

    let mut damage = Damage { points: 0.0, reasons: Vec::new() };
    damage.add(PENDING_PENALTY * pending, pending, "pending sectors");
    damage.add(PENDING_PENALTY * uncorrectable, uncorrectable, "uncorrectable sectors");
    damage.add(realloc_penalty, reallocated, "reallocated sectors");
    damage
}

fn assess_hdd(disk: &Disk, worsening: bool) -> Assessment {
    let mut damage = sector_damage(disk);
    let spindle = attribute(disk, 10);
    damage.add(if spindle > 0.0 { SPINDLE_PENALTY } else { 0.0 }, spindle, "spin retries");

    let score = (100.0 - damage.points).max(0.0);
    let tier = if worsening {
        Tier::Critical
    } else if score < WARNING_BELOW {
        Tier::Warning
    } else if score < 100.0 {
        Tier::Watch
    } else {
        Tier::Healthy
    };
    let mut reasons = damage.reasons;
    if worsening {
        reasons.insert(0, "Damage counters rose in the last 7 days".to_string());
    }
    Assessment { score: Some(score), tier, reasons }
}

fn assess_nvme(disk: &Disk, worsening: bool) -> Assessment {
    let Some(health) = &disk.nvme else {
        return unscored();
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

/// Vendor SATA SSDs expose endurance under different names and IDs. The names
/// are matched first; 231, 233, 202 and 169 are the common IDs, used when
/// smartctl does not know the vendor's attribute name and reports
/// `Unknown_Attribute`.
///
/// A name like `Perc_Rated_Life_Used` says what the *raw* value counts, not the
/// normalized one. Under the SMART convention the normalized `value` is always
/// health left -- 100 when new, falling towards the threshold -- so `100 - value`
/// is the percentage used for every attribute matched here. The raw value is not
/// used because it differs by vendor (a counter, a percentage used, or 0).
fn wear_used(disk: &Disk) -> Option<f64> {
    let mut worst: Option<f64> = None;
    for (id, &value) in &disk.attribute_values {
        if !(1..=100).contains(&value) {
            continue;
        }
        let name = disk
            .attribute_names
            .get(id)
            .map(|name| name.to_ascii_lowercase())
            .unwrap_or_default();
        // Endurance attribute names seen across SSD vendors, lowercased. Some
        // read as "used", but see above: the normalized value is still health
        // left.
        let named = [
            "life_left",
            "wearout_indicator",
            "wear_leveling",
            "lifetime_remain",
            "remaining_lifetime",
            "remaining_life",
            "rated_life_used",
            "lifetime_used",
            "life_used",
        ]
        .iter()
        .any(|pattern| name.contains(pattern));
        let unnamed_common_id =
            (name.is_empty() || name.starts_with("unknown")) && [169, 202, 231, 233].contains(id);
        if !named && !unnamed_common_id {
            continue;
        }
        let used = 100.0 - value as f64;
        worst = Some(worst.map_or(used, |worst| worst.max(used)));
    }
    worst
}

fn assess_sata_ssd(disk: &Disk, worsening: bool) -> Assessment {
    let damage = sector_damage(disk);
    let wear = wear_used(disk);
    let mut reasons = damage.reasons;
    let score = match wear {
        Some(used) => {
            reasons.insert(0, format!("{used:.0}% of rated endurance used"));
            100.0 - used - damage.points
        }
        None if damage.points == 0.0 => {
            return Assessment {
                score: None,
                tier: Tier::Unknown,
                reasons: vec!["SATA SSD wear attributes are not mapped for this drive".to_string()],
            }
        }
        None => {
            reasons.insert(0, "SATA SSD wear attributes are not mapped for this drive".to_string());
            100.0 - damage.points
        }
    };
    let score = score.max(0.0);
    let mut tier = if score < WARNING_BELOW {
        Tier::Warning
    } else if damage.points > 0.0 {
        Tier::Watch
    } else {
        Tier::Healthy
    };
    if worsening {
        tier = Tier::Critical;
        reasons.insert(0, "Damage counters rose in the last 7 days".to_string());
    }
    Assessment { score: Some(score), tier, reasons }
}

fn unscored() -> Assessment {
    Assessment {
        score: None,
        tier: Tier::Unknown,
        reasons: vec!["Drive type is not supported yet".to_string()],
    }
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
            attribute_names: BTreeMap::new(),
            attribute_values: BTreeMap::new(),
            nvme: None,
        }
    }

    /// `wear` is (id, name, normalized value) with a raw value of 0, which is
    /// what real drives report for many of these attributes. `damage` is
    /// (id, raw count).
    fn sata_ssd(wear: &[(u32, &str, i64)], damage: &[(u32, i64)]) -> Disk {
        let mut disk = hdd(damage);
        disk.kind = Kind::SataSsd;
        for (id, name, value) in wear {
            disk.attributes.insert(*id, 0);
            disk.attribute_names.insert(*id, name.to_string());
            disk.attribute_values.insert(*id, *value);
        }
        disk
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

    #[test]
    fn sata_ssd_fixture_scores_from_real_shaped_json() {
        let disk = parse(include_str!("../tests/fixtures/sata_ssd_synthetic.json")).unwrap();
        let result = assess(&disk, false);
        assert_eq!((result.score, result.tier), (Some(88.0), Tier::Healthy));
        assert_eq!(result.reasons[0], "12% of rated endurance used");
    }

    #[test]
    fn sata_ssd_life_left_drives_the_score() {
        let result = assess(&sata_ssd(&[(231, "SSD_Life_Left", 90)], &[]), false);
        assert_eq!((result.score, result.tier), (Some(90.0), Tier::Healthy));
        assert_eq!(result.reasons[0], "10% of rated endurance used");
    }

    #[test]
    fn sata_ssd_vendor_wear_names_are_understood() {
        for (id, name, value, used) in [
            (233, "Media_Wearout_Indicator", 80, 20.0),
            (169, "Remaining_Lifetime_Perc", 95, 5.0),
            (202, "Perc_Rated_Life_Used", 75, 25.0),
            (202, "Percent_Lifetime_Remain", 60, 40.0),
            (177, "Wear_Leveling_Count", 70, 30.0),
        ] {
            let result = assess(&sata_ssd(&[(id, name, value)], &[]), false);
            assert_eq!(result.score, Some(100.0 - used), "{name}");
        }
    }

    #[test]
    fn sata_ssd_common_wear_id_with_unknown_name_is_understood() {
        // smartctl does not know every vendor's attribute name and reports
        // `Unknown_Attribute`; the normalized value is still health left.
        let result = assess(&sata_ssd(&[(169, "Unknown_Attribute", 94)], &[]), false);
        assert_eq!(result.score, Some(94.0));
        assert_eq!(result.reasons[0], "6% of rated endurance used");
    }

    #[test]
    fn sata_ssd_unknown_name_outside_the_common_ids_is_unmapped() {
        for id in [173, 201] {
            let result = assess(&sata_ssd(&[(id, "Unknown_Attribute", 81)], &[]), false);
            assert_eq!(result.tier, Tier::Unknown, "id {id}");
        }
    }

    #[test]
    fn sata_ssd_new_drive_with_zero_raw_is_healthy() {
        // Crucial-style: normalized 100, raw 0. Reading raw as "life left"
        // would score a new drive as fully worn.
        let result = assess(&sata_ssd(&[(202, "Percent_Lifetime_Remain", 100)], &[]), false);
        assert_eq!((result.score, result.tier), (Some(100.0), Tier::Healthy));
    }

    #[test]
    fn sata_ssd_most_worn_attribute_wins() {
        let wear = [(231, "SSD_Life_Left", 95), (233, "Media_Wearout_Indicator", 70)];
        assert_eq!(assess(&sata_ssd(&wear, &[]), false).score, Some(70.0));
    }

    #[test]
    fn sata_ssd_unknown_wear_without_damage_is_unknown() {
        let result = assess(&sata_ssd(&[], &[]), false);
        assert_eq!((result.score, result.tier), (None, Tier::Unknown));
    }

    #[test]
    fn sata_ssd_unknown_wear_still_scores_damage() {
        let result = assess(&sata_ssd(&[], &[(197, 1)]), false);
        assert_eq!((result.score, result.tier), (Some(95.0), Tier::Watch));
        assert!(result.reasons.iter().any(|reason| reason.contains("not mapped")));
    }

    #[test]
    fn sata_ssd_wear_and_damage_stack() {
        let result = assess(&sata_ssd(&[(231, "SSD_Life_Left", 50)], &[(197, 1)]), false);
        assert_eq!((result.score, result.tier), (Some(45.0), Tier::Watch));
    }

    #[test]
    fn sata_ssd_high_wear_is_warning() {
        let result = assess(&sata_ssd(&[(231, "SSD_Life_Left", 10)], &[]), false);
        assert_eq!((result.score, result.tier), (Some(10.0), Tier::Warning));
    }

    #[test]
    fn sata_ssd_worsening_is_critical() {
        let result = assess(&sata_ssd(&[(231, "SSD_Life_Left", 90)], &[]), true);
        assert_eq!((result.score, result.tier), (Some(90.0), Tier::Critical));
    }

    #[test]
    fn sata_ssd_out_of_range_wear_is_ignored() {
        for value in [0, 101, 200, 253] {
            let result = assess(&sata_ssd(&[(231, "SSD_Life_Left", value)], &[]), false);
            assert_eq!(result.tier, Tier::Unknown, "value {value}");
        }
    }

    #[test]
    fn sata_ssd_wear_comes_through_the_smart_json() {
        let json = r#"{
            "device": {"protocol": "ATA"},
            "serial_number": "X",
            "rotation_rate": 0,
            "smart_status": {"passed": true},
            "ata_smart_attributes": {"table": [
                {"id": 202, "name": "Percent_Lifetime_Remain", "value": 92, "raw": {"value": 0}}
            ]}
        }"#;
        let result = assess(&parse(json).unwrap(), false);
        assert_eq!((result.score, result.tier), (Some(92.0), Tier::Healthy));
    }

    #[test]
    fn sata_ssd_attribute_without_a_normalized_value_is_unmapped() {
        let json = r#"{
            "device": {"protocol": "ATA"},
            "serial_number": "X",
            "rotation_rate": 0,
            "ata_smart_attributes": {"table": [
                {"id": 231, "name": "SSD_Life_Left", "raw": {"value": 92}}
            ]}
        }"#;
        assert_eq!(assess(&parse(json).unwrap(), false).tier, Tier::Unknown);
    }
}
