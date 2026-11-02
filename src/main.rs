mod alert;
mod score;
mod smart;
mod store;
mod tui;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::{self, ExitCode};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alert::Settings;
use score::{Assessment, Tier};
use serde_json::json;
use smart::{Disk, Kind};
use store::Store;

const USAGE: &str = "usage: lastwrite [--db PATH] | lastwrite tui|report [--db PATH] | \
                     lastwrite daemon [--db PATH] | lastwrite alert-test";

/// One failed poll can be a glitch; two in a row get an alert.
const MISSING_POLLS: u32 = 2;

/// After a failed send, the daemon polls again this soon instead of waiting
/// out the full interval, so a dying drive's alert is not delayed for hours.
const RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);

enum Mode {
    Default,
    Tui,
    Report,
    Daemon,
    AlertTest,
}

pub(crate) struct Reading {
    pub(crate) disk_id: i64,
    pub(crate) disk: Disk,
    pub(crate) assessment: Assessment,
}

pub(crate) struct Poll {
    pub(crate) readings: Vec<Reading>,
    pub(crate) asleep: Vec<String>,
    pub(crate) all_read: bool,
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(2),
        Err(message) => {
            eprintln!("lastwrite: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Returns false if any drive could not be read.
fn run() -> Result<bool, String> {
    let (mode, db) = parse_args(std::env::args().skip(1).collect())?;
    // A plain run opens the dashboard on a terminal and stays script-friendly
    // when its output is piped.
    let mode = match mode {
        Mode::Default if std::io::stdout().is_terminal() => Mode::Tui,
        Mode::Default => Mode::Report,
        mode => mode,
    };
    match mode {
        Mode::Tui => {
            tui::run(database_path(db)?)?;
            Ok(true)
        }
        Mode::Report => {
            let mut store = Store::open(&database_path(db)?)?;
            let poll = poll(&mut store)?;
            if poll.readings.is_empty() && poll.asleep.is_empty() {
                return Err("no readable drives; try running as root".to_string());
            }
            for reading in &poll.readings {
                print!("{}", render(&reading.disk, &reading.assessment));
            }
            Ok(poll.all_read)
        }
        Mode::Daemon => {
            let settings = Settings::from_env()?;
            let store = Store::open(&database_path(db)?)?;
            daemon(store, settings)
        }
        Mode::AlertTest => alert_test(),
        Mode::Default => unreachable!("default mode is resolved above"),
    }
}

fn parse_args(args: Vec<String>) -> Result<(Mode, Option<PathBuf>), String> {
    let mut args = args.into_iter().peekable();
    let mode = match args.peek().map(String::as_str) {
        Some("daemon") => Mode::Daemon,
        Some("alert-test") => Mode::AlertTest,
        Some("tui") => Mode::Tui,
        Some("report") => Mode::Report,
        _ => Mode::Default,
    };
    if !matches!(mode, Mode::Default) {
        args.next();
    }
    match (args.next().as_deref(), args.next(), args.next()) {
        (None, None, None) => Ok((mode, None)),
        (Some("--db"), Some(path), None) => Ok((mode, Some(PathBuf::from(path)))),
        _ => Err(USAGE.to_string()),
    }
}

fn database_path(custom: Option<PathBuf>) -> Result<PathBuf, String> {
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

pub(crate) fn poll(store: &mut Store) -> Result<Poll, String> {
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

fn daemon(mut store: Store, settings: Settings) -> Result<bool, String> {
    if settings.channels.is_empty() {
        eprintln!("lastwrite: no alert channel set; recording history only");
    }
    eprintln!("lastwrite: polling every {}s", settings.interval.as_secs());
    let host = hostname();
    loop {
        let mut retry = false;
        // One bad poll must not stop monitoring.
        match poll(&mut store) {
            Ok(poll) => {
                eprintln!("lastwrite: read {} drive(s)", poll.readings.len());
                for reading in &poll.readings {
                    match notify(&store, &settings, &host, reading) {
                        Ok(failed) => retry |= failed,
                        Err(message) => eprintln!("lastwrite: {message}"),
                    }
                }
                match check_missing(&store, &settings, &host, &poll) {
                    Ok(failed) => retry |= failed,
                    Err(message) => eprintln!("lastwrite: {message}"),
                }
            }
            Err(message) => eprintln!("lastwrite: {message}"),
        }
        thread::sleep(next_wait(settings.interval, retry));
    }
}

fn next_wait(interval: Duration, retry: bool) -> Duration {
    if retry {
        interval.min(RETRY_INTERVAL)
    } else {
        interval
    }
}

/// True if an alert failed to send and should be retried soon. Having no
/// channel configured is not a failure.
fn send_failed(delivery: &alert::Delivery) -> bool {
    delivery.delivered == 0 && !delivery.failures.is_empty()
}

/// A drive that stops answering may be the failure itself, so it alerts too.
/// Returns true if an alert failed to send.
fn check_missing(
    store: &Store,
    settings: &Settings,
    host: &str,
    poll: &Poll,
) -> Result<bool, String> {
    let mut retry = false;
    for known in store.known_disks().map_err(|e| e.to_string())? {
        if poll.readings.iter().any(|reading| reading.disk_id == known.id) {
            store.clear_missing(known.id).map_err(|e| e.to_string())?;
            continue;
        }
        if poll.asleep.contains(&known.device) {
            continue;
        }
        let (polls, alerted) = store.mark_missing(known.id).map_err(|e| e.to_string())?;
        if polls < MISSING_POLLS || alerted {
            continue;
        }
        let text = format!(
            "lastwrite on {host}\n{} stopped responding at {}.\n  \
             Check the drive and your backups.\n  Serial: {}\n",
            known.model, known.device, known.serial
        );
        let details = json!({
            "host": host,
            "model": known.model,
            "serial": known.serial,
            "tier": "MISSING",
        });
        let delivery = settings.channels.send(&text, details);
        for failure in &delivery.failures {
            eprintln!("lastwrite: alert failed: {failure}");
        }
        if delivery.delivered > 0 {
            store.set_missing_alerted(known.id).map_err(|e| e.to_string())?;
        }
        retry |= send_failed(&delivery);
    }
    Ok(retry)
}

/// Returns true if an alert failed to send and should be retried soon.
fn notify(store: &Store, settings: &Settings, host: &str, reading: &Reading) -> Result<bool, String> {
    let Reading { disk_id, disk, assessment } = reading;
    let state = store.alert_state(*disk_id).map_err(|e| e.to_string())?;
    let decision =
        alert::decide(state, assessment.tier, disk.temperature_c, settings.temp_limit);
    if decision.tier_alert || decision.temp_alert {
        let text = alert_text(host, disk, assessment, decision.temp_alert, settings.temp_limit);
        let delivery = settings.channels.send(&text, details(host, disk, assessment));
        for failure in &delivery.failures {
            eprintln!("lastwrite: alert failed: {failure}");
        }
        // Leaving the state unchanged retries this alert on the next poll.
        if delivery.delivered == 0 {
            return Ok(send_failed(&delivery));
        }
    }
    if decision.next != state {
        store.save_alert_state(*disk_id, decision.next).map_err(|e| e.to_string())?;
    }
    Ok(false)
}

fn alert_text(
    host: &str,
    disk: &Disk,
    assessment: &Assessment,
    temp_alert: bool,
    temp_limit: i64,
) -> String {
    let mut text = format!("lastwrite on {host}\n{}", render(disk, assessment));
    if temp_alert {
        text.push_str(&format!("  Temperature is above the {temp_limit}C limit.\n"));
    }
    text.push_str(&format!("  Serial: {}\n", disk.serial));
    text
}

fn details(host: &str, disk: &Disk, assessment: &Assessment) -> serde_json::Value {
    json!({
        "host": host,
        "model": disk.model,
        "serial": disk.serial,
        "type": disk.kind.label(),
        "score": assessment.score,
        "tier": assessment.tier.label(),
        "temperature_c": disk.temperature_c,
        "reasons": assessment.reasons,
    })
}

fn hostname() -> String {
    process::Command::new("hostname")
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown host".to_string())
}

fn alert_test() -> Result<bool, String> {
    let settings = Settings::from_env()?;
    if settings.channels.is_empty() {
        return Err("no alert channel set; see contrib/lastwrite.env.example".to_string());
    }
    let text = format!("lastwrite on {}\nTest alert. Notifications are working.", hostname());
    let delivery = settings.channels.send(&text, json!({ "test": true }));
    for failure in &delivery.failures {
        eprintln!("alert failed: {failure}");
    }
    println!("{} channel(s) delivered", delivery.delivered);
    Ok(delivery.failures.is_empty())
}

fn render(disk: &Disk, assessment: &Assessment) -> String {
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
pub(crate) fn advice(kind: Kind, tier: Tier) -> Option<&'static str> {
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

    fn delivery(delivered: usize, failures: &[&str]) -> alert::Delivery {
        alert::Delivery { delivered, failures: failures.iter().map(|f| f.to_string()).collect() }
    }

    #[test]
    fn failed_send_shortens_the_wait() {
        let four_hours = Duration::from_secs(4 * 60 * 60);
        assert_eq!(next_wait(four_hours, true), RETRY_INTERVAL);
        assert_eq!(next_wait(four_hours, false), four_hours);
    }

    #[test]
    fn retry_never_waits_longer_than_the_interval() {
        let short = Duration::from_secs(60);
        assert_eq!(next_wait(short, true), short);
    }

    #[test]
    fn only_a_real_failure_triggers_a_retry() {
        assert!(send_failed(&delivery(0, &["webhook: server returned HTTP 500"])));
        assert!(!send_failed(&delivery(0, &[])), "no channel configured");
        assert!(!send_failed(&delivery(1, &[])));
        assert!(!send_failed(&delivery(1, &["telegram: request failed"])), "one channel got it");
    }
}
