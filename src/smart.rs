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
    /// Normalized values (higher is better), kept apart from raw counters.
    pub attribute_values: BTreeMap<u32, i64>,
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
    #[serde(default)]
    value: Option<i64>,
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
    let mut devices = scan.devices;
    add_missed_usb_disks(&mut devices);
    Ok(devices)
}

pub fn read(device: &ScanDevice) -> Result<Disk, String> {
    // `-n standby` keeps polling from spinning up a sleeping HDD.
    let mut disk = run_smartctl(device, &device.device_type)?;
    // `--scan` reports a SATA disk behind an HBA as `scsi`, and reading it
    // that way yields a serial but no SMART data. The SAT layer can still read
    // it, so retry when the first read produced nothing usable.
    if needs_sat_retry(&device.device_type, &disk) {
        if let Ok(retried) = run_smartctl(device, "sat") {
            if retried.kind != Kind::Unknown || !retried.attributes.is_empty() {
                disk = retried;
            }
        }
    }
    disk.device = device.name.clone();
    Ok(disk)
}

fn run_smartctl(device: &ScanDevice, device_type: &str) -> Result<Disk, String> {
    let output = Command::new("smartctl")
        .args(["-a", "-j", "-n", "standby", "-d", device_type, &device.name])
        .output()
        .map_err(|e| format!("cannot run smartctl: {e}"))?;
    // The exit code is a bitmask that can be non-zero on good data, so the
    // JSON decides whether the read worked.
    parse(&String::from_utf8_lossy(&output.stdout))
}

fn needs_sat_retry(device_type: &str, disk: &Disk) -> bool {
    device_type == "scsi" && disk.kind == Kind::Unknown && disk.attributes.is_empty()
}

/// True for whole-disk names like `sda` and `sdab`, false for partitions,
/// NVMe names and device-mapper nodes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn is_sd_name(name: &str) -> bool {
    name.strip_prefix("sd")
        .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_lowercase()))
}

/// `smartctl --scan` silently omits some USB bridges even though `-d sat`
/// reads them. Every USB mass-storage disk shows up as an `sd` node in sysfs,
/// so add the ones the scan missed and let `read` negotiate the protocol.
#[cfg(target_os = "linux")]
fn add_missed_usb_disks(devices: &mut Vec<ScanDevice>) {
    let Ok(entries) = std::fs::read_dir("/sys/block") else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_sd_name(name) {
            continue;
        }
        let is_usb = std::fs::canonicalize(entry.path().join("device"))
            .is_ok_and(|path| path.to_string_lossy().contains("/usb"));
        if !is_usb {
            continue;
        }
        let device = format!("/dev/{name}");
        if devices.iter().any(|known| known.name == device) {
            continue;
        }
        devices.push(ScanDevice { name: device, device_type: "sat".to_string() });
    }
}

#[cfg(not(target_os = "linux"))]
fn add_missed_usb_disks(_devices: &mut Vec<ScanDevice>) {}

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
    let mut attributes = BTreeMap::new();
    let mut attribute_names = BTreeMap::new();
    let mut attribute_values = BTreeMap::new();
    for attribute in report.ata_smart_attributes.map(|ata| ata.table).unwrap_or_default() {
        attributes.insert(attribute.id, attribute.raw.value);
        if !attribute.name.is_empty() {
            attribute_names.insert(attribute.id, attribute.name);
        }
        if let Some(value) = attribute.value {
            attribute_values.insert(attribute.id, value);
        }
    }
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
        attribute_values,
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
    fn parses_sata_ssd_attributes() {
        let disk = parse(include_str!("../tests/fixtures/sata_ssd_synthetic.json")).unwrap();
        assert_eq!(disk.kind, Kind::SataSsd);
        assert_eq!(disk.model, "CT1000MX500SSD1");
        assert_eq!(disk.attribute_names[&5], "Reallocated_Sector_Ct");
        assert_eq!(disk.attributes[&202], 0);
        assert_eq!(disk.attribute_values[&202], 88);
        assert_eq!(disk.capacity_bytes, Some(1000204886016));
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

    #[test]
    fn sat_retry_only_for_unusable_scsi_reads() {
        let scsi = parse(
            r#"{"device":{"protocol":"SCSI"},"serial_number":"X","rotation_rate":0}"#,
        )
        .unwrap();
        assert!(needs_sat_retry("scsi", &scsi));
        assert!(!needs_sat_retry("sat", &scsi));
        assert!(!needs_sat_retry("nvme", &scsi));

        let sata = parse(include_str!("../tests/fixtures/sata_ssd_synthetic.json")).unwrap();
        assert!(!needs_sat_retry("scsi", &sata));
    }

    #[test]
    fn sd_names_are_whole_disks_only() {
        assert!(is_sd_name("sda"));
        assert!(is_sd_name("sdab"));
        assert!(!is_sd_name("sd"));
        assert!(!is_sd_name("sda1"));
        assert!(!is_sd_name("nvme0n1"));
        assert!(!is_sd_name("dm-0"));
    }
}
