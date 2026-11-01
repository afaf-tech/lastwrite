//! Reads drive data from `smartctl` JSON output.

use std::collections::BTreeMap;
use std::process::Command;

use serde::Deserialize;

/// History key for NVMe media errors, above the ATA attribute range.
pub const NVME_MEDIA_ERRORS_ID: u32 = 1000;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Kind {
    Hdd,
    SataSsd,
    Nvme,
    Unknown,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Hdd => "HDD",
            Kind::SataSsd => "SATA SSD",
            Kind::Nvme => "NVMe",
            Kind::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct NvmeHealth {
    pub percentage_used: u64,
    pub available_spare: u64,
    pub available_spare_threshold: u64,
    #[serde(default)]
    pub critical_warning: u64,
    #[serde(default)]
    pub media_errors: i64,
}

#[derive(Debug)]
pub struct Disk {
    pub device: String,
    pub serial: String,
    pub model: String,
    pub firmware: Option<String>,
    pub capacity_bytes: Option<u64>,
    pub kind: Kind,
    pub temperature_c: Option<i64>,
    pub smart_passed: Option<bool>,
    pub attributes: BTreeMap<u32, i64>,
    pub attribute_names: BTreeMap<u32, String>,
    pub nvme: Option<NvmeHealth>,
}

#[derive(Debug, Deserialize)]
pub struct ScanDevice {
    pub name: String,
    #[serde(rename = "type")]
    pub device_type: String,
}

#[derive(Deserialize)]
struct Scan {
    #[serde(default)]
    devices: Vec<ScanDevice>,
}

#[derive(Deserialize)]
struct Report {
    smartctl: Option<Tool>,
    device: Option<Device>,
    model_name: Option<String>,
    serial_number: Option<String>,
    firmware_version: Option<String>,
    user_capacity: Option<Capacity>,
    rotation_rate: Option<u64>,
    smart_status: Option<Status>,
    temperature: Option<Temperature>,
    ata_smart_attributes: Option<AtaTable>,
    nvme_smart_health_information_log: Option<NvmeHealth>,
}

#[derive(Deserialize)]
struct Tool {
    #[serde(default)]
    messages: Vec<Message>,
}

#[derive(Deserialize)]
struct Message {
    string: String,
}

#[derive(Deserialize)]
struct Device {
    protocol: String,
}

#[derive(Deserialize)]
struct Capacity {
    bytes: u64,
}

#[derive(Deserialize)]
struct Status {
    passed: bool,
}

#[derive(Deserialize)]
struct Temperature {
    current: i64,
}

#[derive(Deserialize)]
struct AtaTable {
    table: Vec<AtaAttribute>,
}

#[derive(Deserialize)]
struct AtaAttribute {
    id: u32,
    #[serde(default)]
    name: String,
    raw: AtaRaw,
}

#[derive(Deserialize)]
struct AtaRaw {
    value: i64,
}

pub fn scan() -> Result<Vec<ScanDevice>, String> {
    let output = Command::new("smartctl")
        .args(["--scan", "-j"])
        .output()
        .map_err(|e| format!("cannot run smartctl ({e}); install smartmontools"))?;
    let scan: Scan = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("cannot parse smartctl scan: {e}"))?;
    Ok(scan.devices)
}

pub fn read(device: &ScanDevice) -> Result<Disk, String> {
    // `-n standby` keeps polling from spinning up a sleeping HDD.
    let output = Command::new("smartctl")
        .args(["-a", "-j", "-n", "standby", "-d", &device.device_type, &device.name])
        .output()
        .map_err(|e| format!("cannot run smartctl: {e}"))?;
    // The exit code is a bitmask that can be non-zero on good data, so the
    // JSON decides whether the read worked.
    let mut disk = parse(&String::from_utf8_lossy(&output.stdout))?;
    disk.device = device.name.clone();
    Ok(disk)
}

/// A sleeping drive is fine; `-n standby` makes smartctl skip it.
/// The wording varies between smartctl versions and platforms, so match it
/// case-insensitively instead of expecting one exact string.
pub fn is_asleep(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("standby") || message.contains("sleep")
}

pub fn parse(json: &str) -> Result<Disk, String> {
    let report: Report =
        serde_json::from_str(json).map_err(|e| format!("cannot parse smartctl output: {e}"))?;
    let Some(serial) = report.serial_number else {
        let messages: Vec<String> = report
            .smartctl
            .map(|tool| tool.messages.into_iter().map(|m| m.string).collect())
            .unwrap_or_default();
        return Err(if messages.is_empty() {
            "smartctl returned no drive data".to_string()
        } else {
            messages.join("; ")
        });
    };
    let protocol = report.device.map(|d| d.protocol).unwrap_or_default();
    let kind = match (protocol.as_str(), report.rotation_rate) {
        ("NVMe", _) => Kind::Nvme,
        ("ATA", Some(0)) => Kind::SataSsd,
        ("ATA", Some(_)) => Kind::Hdd,
        _ => Kind::Unknown,
    };
    let (mut attributes, attribute_names): (BTreeMap<u32, i64>, BTreeMap<u32, String>) = report
        .ata_smart_attributes
        .map(|ata| {
            let mut attributes = BTreeMap::new();
            let mut names = BTreeMap::new();
            for attribute in ata.table {
                attributes.insert(attribute.id, attribute.raw.value);
                if !attribute.name.is_empty() {
                    names.insert(attribute.id, attribute.name);
                }
            }
            (attributes, names)
        })
        .unwrap_or_default();
    if let Some(health) = &report.nvme_smart_health_information_log {
        attributes.insert(NVME_MEDIA_ERRORS_ID, health.media_errors);
    }
    Ok(Disk {
        device: String::new(),
        serial,
        model: report.model_name.unwrap_or_else(|| "Unknown model".to_string()),
        firmware: report.firmware_version,
        capacity_bytes: report.user_capacity.map(|c| c.bytes),
        kind,
        temperature_c: report.temperature.map(|t| t.current),
        smart_passed: report.smart_status.map(|s| s.passed),
        attributes,
        attribute_names,
        nvme: report.nvme_smart_health_information_log,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_nvme_output_with_nonzero_exit() {
        let disk = parse(include_str!("../tests/fixtures/nvme_apple.json")).unwrap();
        assert_eq!(disk.kind, Kind::Nvme);
        assert_eq!(disk.model, "APPLE SSD AP1024Q");
        assert!(disk.temperature_c.is_some());
        assert_eq!(disk.attributes[&NVME_MEDIA_ERRORS_ID], 0);
        assert_eq!(disk.nvme.unwrap().available_spare_threshold, 99);
    }

    #[test]
    fn parses_hdd_attributes() {
        let disk = parse(include_str!("../tests/fixtures/hdd_synthetic.json")).unwrap();
        assert_eq!(disk.kind, Kind::Hdd);
        assert_eq!(disk.attributes[&5], 12);
        assert_eq!(disk.attribute_names[&5], "Reallocated_Sector_Ct");
        assert_eq!(disk.capacity_bytes, Some(4000787030016));
    }

    #[test]
    fn standby_and_sleep_messages_are_asleep() {
        for message in [
            "Device is in STANDBY mode, exit(2)",
            "Device is in standby mode",
            "device is in sleep mode",
        ] {
            assert!(is_asleep(message), "{message}");
        }
        assert!(!is_asleep("SMART support is: Unavailable"));
    }

    #[test]
    fn rotation_rate_zero_is_sata_ssd() {
        let json = r#"{"device":{"protocol":"ATA"},"serial_number":"X","rotation_rate":0}"#;
        assert_eq!(parse(json).unwrap().kind, Kind::SataSsd);
    }

    #[test]
    fn missing_serial_reports_smartctl_message() {
        let json = r#"{"smartctl":{"messages":[{"string":"Device is in STANDBY mode"}]}}"#;
        assert_eq!(parse(json).unwrap_err(), "Device is in STANDBY mode");
    }
}
