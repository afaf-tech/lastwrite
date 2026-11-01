//! Daemon settings, alert decisions and notification delivery.

use std::env;
use std::time::Duration;

use serde_json::{json, Value};
use ureq::Agent;

use crate::score::Tier;

const DEFAULT_INTERVAL_SECS: u64 = 4 * 60 * 60;
const DEFAULT_TEMP_LIMIT: i64 = 55;
/// A hot drive must cool this far below the limit before it can alert again.
const TEMP_RESET_MARGIN: i64 = 5;
const SEND_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Settings {
    pub interval: Duration,
    pub temp_limit: i64,
    pub channels: Channels,
}

#[derive(Default)]
pub struct Channels {
    pub webhook_url: Option<String>,
    pub telegram: Option<Telegram>,
}

pub struct Telegram {
    pub token: String,
    pub chat_id: String,
}

impl Settings {
    pub fn from_env() -> Result<Settings, String> {
        Settings::from_lookup(|key| env::var(key).ok().filter(|value| !value.is_empty()))
    }

    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Settings, String> {
        let number = |key: &str, default: u64| match get(key) {
            Some(value) => value
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| format!("{key} must be a positive whole number")),
            None => Ok(default),
        };
        let telegram = match (get("LASTWRITE_TELEGRAM_TOKEN"), get("LASTWRITE_TELEGRAM_CHAT_ID")) {
            (Some(token), Some(chat_id)) => Some(Telegram { token, chat_id }),
            (None, None) => None,
            _ => {
                return Err("set both LASTWRITE_TELEGRAM_TOKEN and LASTWRITE_TELEGRAM_CHAT_ID"
                    .to_string())
            }
        };
        Ok(Settings {
            interval: Duration::from_secs(number(
                "LASTWRITE_INTERVAL_SECONDS",
                DEFAULT_INTERVAL_SECS,
            )?),
            temp_limit: number("LASTWRITE_TEMP_LIMIT", DEFAULT_TEMP_LIMIT as u64)? as i64,
            channels: Channels { webhook_url: get("LASTWRITE_WEBHOOK_URL"), telegram },
        })
    }
}

/// What has already been reported for a drive.
#[derive(Debug, PartialEq, Clone, Copy, Default)]
pub struct AlertState {
    pub rank: u8,
    pub hot: bool,
}

#[derive(Debug, PartialEq)]
pub struct Decision {
    pub tier_alert: bool,
    pub temp_alert: bool,
    pub next: AlertState,
}

fn rank(tier: Tier) -> u8 {
    match tier {
        Tier::Critical => 2,
        Tier::Warning => 1,
        Tier::Watch | Tier::Healthy | Tier::Unknown => 0,
    }
}

/// Alerts fire when a drive moves into a worse state, not on every poll.
pub fn decide(
    state: AlertState,
    tier: Tier,
    temperature_c: Option<i64>,
    temp_limit: i64,
) -> Decision {
    let current = rank(tier);
    let hot = match temperature_c {
        Some(temperature) if temperature > temp_limit => true,
        Some(temperature) if temperature <= temp_limit - TEMP_RESET_MARGIN => false,
        _ => state.hot,
    };
    Decision {
        tier_alert: current > state.rank,
        temp_alert: hot && !state.hot,
        next: AlertState { rank: current, hot },
    }
}

#[derive(Debug)]
pub struct Delivery {
    pub delivered: usize,
    pub failures: Vec<String>,
}

impl Channels {
    pub fn is_empty(&self) -> bool {
        self.webhook_url.is_none() && self.telegram.is_none()
    }

    pub fn send(&self, text: &str, details: Value) -> Delivery {
        let agent: Agent = Agent::config_builder()
            .timeout_global(Some(SEND_TIMEOUT))
            .build()
            .into();
        let mut delivery = Delivery { delivered: 0, failures: Vec::new() };
        if let Some(url) = &self.webhook_url {
            // `text` suits Slack and `content` suits Discord.
            let body = json!({ "text": text, "content": text, "lastwrite": details });
            delivery.record("webhook", post(&agent, url, &body));
        }
        if let Some(telegram) = &self.telegram {
            let url = format!("https://api.telegram.org/bot{}/sendMessage", telegram.token);
            let body = json!({ "chat_id": telegram.chat_id, "text": text });
            delivery.record("telegram", post(&agent, &url, &body));
        }
        delivery
    }
}

impl Delivery {
    fn record(&mut self, channel: &str, result: Result<(), String>) {
        match result {
            Ok(()) => self.delivered += 1,
            Err(reason) => self.failures.push(format!("{channel}: {reason}")),
        }
    }
}

/// Errors never include the URL: webhook URLs and bot tokens are secrets.
fn post(agent: &Agent, url: &str, body: &Value) -> Result<(), String> {
    match agent.post(url).send_json(body) {
        Ok(_) => Ok(()),
        Err(ureq::Error::StatusCode(code)) => Err(format!("server returned HTTP {code}")),
        Err(ureq::Error::Timeout(_)) => Err("request timed out".to_string()),
        Err(_) => Err("request failed".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn settings(values: &[(&str, &str)]) -> Result<Settings, String> {
        let map: HashMap<_, _> = values.iter().copied().collect();
        Settings::from_lookup(|key| map.get(key).map(|value| value.to_string()))
    }

    fn state(rank: u8, hot: bool) -> AlertState {
        AlertState { rank, hot }
    }

    /// Answers one request and returns what it received.
    fn server(status: &'static str) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut received = Vec::new();
            let mut chunk = [0; 4096];
            loop {
                let size = stream.read(&mut chunk).unwrap();
                received.extend_from_slice(&chunk[..size]);
                let text = String::from_utf8_lossy(&received);
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let length = head
                        .lines()
                        .find_map(|l| l.to_lowercase().strip_prefix("content-length: ")
                            .map(|v| v.trim().parse::<usize>().unwrap()))
                        .unwrap_or(0);
                    if body.len() >= length {
                        break;
                    }
                }
            }
            let reply = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            stream.write_all(reply.as_bytes()).unwrap();
            String::from_utf8_lossy(&received).to_string()
        });
        (url, handle)
    }

    #[test]
    fn defaults_apply() {
        let parsed = settings(&[]).unwrap();
        assert_eq!(parsed.interval, Duration::from_secs(14400));
        assert_eq!(parsed.temp_limit, 55);
        assert!(parsed.channels.is_empty());
    }

    #[test]
    fn half_configured_telegram_is_rejected() {
        assert!(settings(&[("LASTWRITE_TELEGRAM_TOKEN", "t")]).is_err());
    }

    #[test]
    fn zero_interval_is_rejected() {
        assert!(settings(&[("LASTWRITE_INTERVAL_SECONDS", "0")]).is_err());
    }

    #[test]
    fn healthy_and_watch_never_alert() {
        for tier in [Tier::Healthy, Tier::Watch, Tier::Unknown] {
            assert!(!decide(state(0, false), tier, Some(40), 55).tier_alert);
        }
    }

    #[test]
    fn entering_warning_or_critical_alerts_once() {
        let first = decide(state(0, false), Tier::Critical, None, 55);
        assert!(first.tier_alert);
        assert!(!decide(first.next, Tier::Critical, None, 55).tier_alert);
    }

    #[test]
    fn warning_to_critical_alerts_again() {
        assert!(decide(state(1, false), Tier::Critical, None, 55).tier_alert);
    }

    #[test]
    fn improving_is_silent_and_a_new_decline_alerts() {
        let better = decide(state(2, false), Tier::Warning, None, 55);
        assert!(!better.tier_alert);
        assert!(decide(better.next, Tier::Critical, None, 55).tier_alert);
    }

    #[test]
    fn temperature_alerts_once_and_needs_cooling_to_rearm() {
        let hot = decide(state(0, false), Tier::Healthy, Some(56), 55);
        assert!(hot.temp_alert);
        let flapping = decide(hot.next, Tier::Healthy, Some(54), 55);
        assert!(flapping.next.hot);
        let again = decide(flapping.next, Tier::Healthy, Some(56), 55);
        assert!(!again.temp_alert);
        let cooled = decide(again.next, Tier::Healthy, Some(50), 55);
        assert!(!cooled.next.hot);
        assert!(decide(cooled.next, Tier::Healthy, Some(56), 55).temp_alert);
    }

    #[test]
    fn webhook_receives_text_and_details() {
        let (url, handle) = server("200 OK");
        let channels = Channels { webhook_url: Some(url), telegram: None };
        let delivery = channels.send("drive is failing", json!({ "tier": "CRITICAL" }));
        assert_eq!(delivery.delivered, 1);
        let request = handle.join().unwrap();
        assert!(request.contains("drive is failing"));
        assert!(request.contains("CRITICAL"));
    }

    #[test]
    fn server_error_is_a_failure() {
        let (url, handle) = server("500 Internal Server Error");
        let channels = Channels { webhook_url: Some(url), telegram: None };
        let delivery = channels.send("x", json!({}));
        handle.join().unwrap();
        assert_eq!(delivery.delivered, 0);
        assert_eq!(delivery.failures, vec!["webhook: server returned HTTP 500"]);
    }

    #[test]
    fn failure_message_does_not_leak_the_url() {
        let url = "http://127.0.0.1:1/secret-token".to_string();
        let channels = Channels { webhook_url: Some(url), telegram: None };
        let delivery = channels.send("x", json!({}));
        assert!(!delivery.failures[0].contains("secret-token"));
    }
}
