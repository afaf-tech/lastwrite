//! Core engine shared by the terminal, desktop and daemon modes.

pub mod alert;
#[cfg(feature = "gui")]
pub mod gui;
pub mod score;
pub mod smart;
pub mod store;
pub mod tui;
pub mod view;

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use score::{Assessment, Tier};
use smart::{Disk, Kind};
use store::Store;

pub struct Reading {
    pub disk_id: i64,
    pub disk: Disk,
    pub assessment: Assessment,
}

pub struct Poll {
    pub readings: Vec<Reading>,
    pub asleep: Vec<String>,
    pub all_read: bool,
}

pub fn database_path(custom: Option<PathBuf>) -> Result<PathBuf, String> {
    match custom {
        Some(path) => Ok(path),
        None => dirs::config_dir()
            .map(|dir| dir.join("lastwrite").join("lastwrite.sqlite"))
            .ok_or_else(|| "cannot find the config directory".to_string()),
    }
}

fn now() -> Result<i64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .map_err(|e| e.to_string())
}

pub fn poll(store: &mut Store) -> Result<Poll, String> {
    let now = now()?;
    let devices = smart::scan()?;
    let mut readings = Vec::new();
    let mut asleep = Vec::new();
    let mut all_read = true;
    for device in &devices {
        match smart::read(device) {
            Ok(disk) => {
                let disk_id = store.upsert_disk(&disk).map_err(|e| e.to_string())?;
                let worsening =
                    store.is_worsening(disk_id, &disk, now).map_err(|e| e.to_string())?;
                let assessment = score::assess(&disk, worsening);
                store
                    .save_snapshot(disk_id, &disk, &assessment, worsening, now)
                    .map_err(|e| e.to_string())?;
                readings.push(Reading { disk_id, disk, assessment });
            }
            Err(message) if smart::is_asleep(&message) => asleep.push(device.name.clone()),
            Err(message) => {
                all_read = false;
                eprintln!("{}: {message}", device.name);
            }
        }
    }
    Ok(Poll { readings, asleep, all_read })
}

pub fn render(disk: &Disk, assessment: &Assessment) -> String {
    let temperature = match disk.temperature_c {
        Some(temperature) => format!(", {temperature}C"),
        None => String::new(),
    };
    let mut text = format!(
        "{} ({}): {}, {}{temperature}\n",
        disk.model,
        disk.kind.label(),
        score_text(assessment),
        assessment.tier.label()
    );
    push_details(&mut text, disk, assessment);
    text
}

/// Alert body for a drive over the temperature limit. The score and tier move
/// below the headline so a hot drive cannot read as "Healthy" at first glance.
pub fn render_hot(disk: &Disk, assessment: &Assessment, temp_limit: i64) -> String {
    let temperature = match disk.temperature_c {
        Some(temperature) => format!("{temperature}C"),
        None => "?".to_string(),
    };
    let mut text = format!(
        "{} ({}): {temperature}, above the {temp_limit}C limit\n  {}, {}\n",
        disk.model,
        disk.kind.label(),
        score_text(assessment),
        assessment.tier.label()
    );
    push_details(&mut text, disk, assessment);
    text
}

fn score_text(assessment: &Assessment) -> String {
    match assessment.score {
        Some(score) => format!("{:.0}%", score.floor()),
        None => "n/a".to_string(),
    }
}

fn push_details(text: &mut String, disk: &Disk, assessment: &Assessment) {
    for reason in &assessment.reasons {
        text.push_str(&format!("  - {reason}\n"));
    }
    if let Some(advice) = advice(disk.kind, assessment.tier) {
        text.push_str(&format!("  {advice}\n"));
    }
}

/// Failure odds come from `backtest/` and only apply to HDDs.
pub fn advice(kind: Kind, tier: Tier) -> Option<&'static str> {
    match (kind, tier) {
        (Kind::Hdd, Tier::Critical) => Some(
            "Between 1 in 9 and 1 in 6 drives like this fail within 60 days. \
             Back up and plan a replacement.",
        ),
        (Kind::Hdd, Tier::Warning) => {
            Some("About 1 in 20 drives like this fails within 60 days. Check your backups.")
        }
        (Kind::Hdd, Tier::Watch) => Some("About 1 in 70 drives like this fails within 60 days."),
        (_, Tier::Critical) => Some("Back up now and plan a replacement."),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn disk(kind: Kind, temperature_c: Option<i64>) -> Disk {
        Disk {
            device: "/dev/test".to_string(),
            serial: "SERIAL".to_string(),
            model: "Test Drive".to_string(),
            firmware: None,
            capacity_bytes: None,
            kind,
            temperature_c,
            smart_passed: Some(true),
            attributes: BTreeMap::new(),
            attribute_names: BTreeMap::new(),
            attribute_values: BTreeMap::new(),
            nvme: None,
        }
    }

    fn assessment() -> Assessment {
        Assessment {
            score: Some(88.0),
            tier: Tier::Healthy,
            reasons: vec!["12% of rated endurance used".to_string()],
        }
    }

    #[test]
    fn render_leads_with_score_and_tier() {
        let text = render(&disk(Kind::SataSsd, Some(70)), &assessment());
        assert_eq!(
            text,
            "Test Drive (SATA SSD): 88%, Healthy, 70C\n  - 12% of rated endurance used\n"
        );
    }

    #[test]
    fn hot_render_leads_with_temperature() {
        let text = render_hot(&disk(Kind::SataSsd, Some(70)), &assessment(), 55);
        assert_eq!(
            text,
            "Test Drive (SATA SSD): 70C, above the 55C limit\n  88%, Healthy\n  \
             - 12% of rated endurance used\n"
        );
    }

    #[test]
    fn hot_render_keeps_advice_for_critical_drives() {
        let mut assessment = assessment();
        assessment.tier = Tier::Critical;
        let text = render_hot(&disk(Kind::Hdd, Some(60)), &assessment, 55);
        assert!(text.contains("fail within 60 days"));
    }
}
