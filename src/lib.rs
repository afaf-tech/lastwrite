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
    let score = match assessment.score {
        Some(score) => format!("{:.0}%", score.floor()),
        None => "n/a".to_string(),
    };
    let temperature = match disk.temperature_c {
        Some(temperature) => format!(", {temperature}C"),
        None => String::new(),
    };
    let mut text = format!(
        "{} ({}): {score}, {}{temperature}\n",
        disk.model,
        disk.kind.label(),
        assessment.tier.label()
    );
    for reason in &assessment.reasons {
        text.push_str(&format!("  - {reason}\n"));
    }
    if let Some(advice) = advice(disk.kind, assessment.tier) {
        text.push_str(&format!("  {advice}\n"));
    }
    text
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
