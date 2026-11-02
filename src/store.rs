//! SQLite history, using the schema from the TRD.

use std::fs;
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension, Result};

use crate::alert::AlertState;
use crate::score::{Assessment, CRITICAL_IDS};
use crate::smart::Disk;

/// Stored in the database's `user_version`. Raise it together with a migration
/// step in `Store::open` whenever an existing table changes.
const SCHEMA_VERSION: i64 = 1;

const WORSENING_WINDOW_SECS: i64 = 7 * 24 * 60 * 60;

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS disks (
        id INTEGER PRIMARY KEY,
        serial_number TEXT NOT NULL UNIQUE,
        model_name TEXT NOT NULL,
        capacity_bytes INTEGER,
        firmware_version TEXT,
        disk_type TEXT NOT NULL,
        device_name TEXT NOT NULL DEFAULT ''
    );
    CREATE TABLE IF NOT EXISTS health_snapshots (
        id INTEGER PRIMARY KEY,
        disk_id INTEGER NOT NULL REFERENCES disks(id),
        timestamp INTEGER NOT NULL,
        health_score REAL,
        performance_score REAL,
        temperature_c INTEGER,
        risk_tier TEXT NOT NULL,
        is_worsening INTEGER NOT NULL,
        estimated_days_low INTEGER,
        estimated_days_high INTEGER
    );
    CREATE TABLE IF NOT EXISTS smart_raw_attributes (
        id INTEGER PRIMARY KEY,
        snapshot_id INTEGER NOT NULL REFERENCES health_snapshots(id),
        attribute_id INTEGER NOT NULL,
        raw_value INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS alert_state (
        disk_id INTEGER PRIMARY KEY REFERENCES disks(id),
        tier_rank INTEGER NOT NULL,
        is_hot INTEGER NOT NULL,
        missing_polls INTEGER NOT NULL DEFAULT 0,
        missing_alerted INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX IF NOT EXISTS idx_snapshots_disk_time
        ON health_snapshots(disk_id, timestamp);
    CREATE INDEX IF NOT EXISTS idx_raw_snapshot
        ON smart_raw_attributes(snapshot_id, attribute_id);
";

pub struct Store {
    conn: Connection,
}

pub struct KnownDisk {
    pub id: i64,
    pub model: String,
    pub serial: String,
    pub device: String,
}

pub struct Snapshot {
    pub timestamp: i64,
    pub score: Option<f64>,
    pub temperature_c: Option<i64>,
    pub worsening: bool,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store, String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|e| e.to_string())?;
        if version > SCHEMA_VERSION {
            return Err(format!(
                "{} was written by a newer lastwrite (schema {version}, this build \
                 understands {SCHEMA_VERSION}); upgrade lastwrite",
                path.display()
            ));
        }
        conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION).map_err(|e| e.to_string())?;
        Ok(Store { conn })
    }

    pub fn upsert_disk(&self, disk: &Disk) -> Result<i64> {
        self.conn.query_row(
            "INSERT INTO disks
                 (serial_number, model_name, capacity_bytes, firmware_version, disk_type,
                  device_name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(serial_number) DO UPDATE SET
                 model_name = excluded.model_name,
                 capacity_bytes = excluded.capacity_bytes,
                 firmware_version = excluded.firmware_version,
                 disk_type = excluded.disk_type,
                 device_name = excluded.device_name
             RETURNING id",
            params![
                disk.serial,
                disk.model,
                disk.capacity_bytes.map(|b| b as i64),
                disk.firmware,
                disk.kind.label(),
                disk.device
            ],
            |row| row.get(0),
        )
    }

    /// True if a damage counter is above its lowest value in the last 7 days.
    /// Call before saving the current reading.
    pub fn is_worsening(&self, disk_id: i64, disk: &Disk, now: i64) -> Result<bool> {
        let mut lowest = self.conn.prepare_cached(
            "SELECT MIN(a.raw_value)
             FROM smart_raw_attributes a
             JOIN health_snapshots s ON s.id = a.snapshot_id
             WHERE s.disk_id = ?1 AND s.timestamp >= ?2 AND a.attribute_id = ?3",
        )?;
        for id in CRITICAL_IDS {
            let Some(current) = disk.attributes.get(&id) else { continue };
            let low: Option<i64> = lowest.query_row(
                params![disk_id, now - WORSENING_WINDOW_SECS, id],
                |row| row.get(0),
            )?;
            if low.is_some_and(|low| *current > low) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn save_snapshot(
        &mut self,
        disk_id: i64,
        disk: &Disk,
        assessment: &Assessment,
        worsening: bool,
        now: i64,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO health_snapshots
                 (disk_id, timestamp, health_score, temperature_c, risk_tier, is_worsening)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                disk_id,
                now,
                assessment.score,
                disk.temperature_c,
                assessment.tier.label(),
                worsening
            ],
        )?;
        let snapshot_id = tx.last_insert_rowid();
        for (id, value) in &disk.attributes {
            tx.execute(
                "INSERT INTO smart_raw_attributes (snapshot_id, attribute_id, raw_value)
                 VALUES (?1, ?2, ?3)",
                params![snapshot_id, id, value],
            )?;
        }
        tx.commit()
    }

    pub fn alert_state(&self, disk_id: i64) -> Result<AlertState> {
        let state = self
            .conn
            .query_row(
                "SELECT tier_rank, is_hot FROM alert_state WHERE disk_id = ?1",
                [disk_id],
                |row| Ok(AlertState { rank: row.get(0)?, hot: row.get(1)? }),
            )
            .optional()?;
        Ok(state.unwrap_or_default())
    }

    /// Newest `limit` snapshots, returned oldest first for a left-to-right trend.
    pub fn history(&self, disk_id: i64, limit: i64) -> Result<Vec<Snapshot>> {
        let mut query = self.conn.prepare_cached(
            "SELECT timestamp, health_score, temperature_c, is_worsening
             FROM health_snapshots
             WHERE disk_id = ?1
             ORDER BY timestamp DESC
             LIMIT ?2",
        )?;
        let mut snapshots = query
            .query_map(params![disk_id, limit], |row| {
                Ok(Snapshot {
                    timestamp: row.get(0)?,
                    score: row.get(1)?,
                    temperature_c: row.get(2)?,
                    worsening: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>>>()?;
        snapshots.reverse();
        Ok(snapshots)
    }

    pub fn known_disks(&self) -> Result<Vec<KnownDisk>> {
        let mut query = self
            .conn
            .prepare_cached("SELECT id, model_name, serial_number, device_name FROM disks")?;
        let disks = query.query_map([], |row| {
            Ok(KnownDisk {
                id: row.get(0)?,
                model: row.get(1)?,
                serial: row.get(2)?,
                device: row.get(3)?,
            })
        })?;
        disks.collect()
    }

    /// Counts one more poll without this drive. Returns the running total and
    /// whether the alert has already gone out.
    pub fn mark_missing(&self, disk_id: i64) -> Result<(u32, bool)> {
        self.conn.query_row(
            "INSERT INTO alert_state (disk_id, tier_rank, is_hot, missing_polls)
             VALUES (?1, 0, 0, 1)
             ON CONFLICT(disk_id) DO UPDATE SET missing_polls = missing_polls + 1
             RETURNING missing_polls, missing_alerted",
            [disk_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
    }

    pub fn set_missing_alerted(&self, disk_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE alert_state SET missing_alerted = 1 WHERE disk_id = ?1",
            [disk_id],
        )?;
        Ok(())
    }

    pub fn clear_missing(&self, disk_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE alert_state SET missing_polls = 0, missing_alerted = 0 WHERE disk_id = ?1",
            [disk_id],
        )?;
        Ok(())
    }

    pub fn save_alert_state(&self, disk_id: i64, state: AlertState) -> Result<()> {
        self.conn.execute(
            "INSERT INTO alert_state (disk_id, tier_rank, is_hot) VALUES (?1, ?2, ?3)
             ON CONFLICT(disk_id) DO UPDATE SET
                 tier_rank = excluded.tier_rank,
                 is_hot = excluded.is_hot",
            params![disk_id, state.rank, state.hot],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::score::assess;
    use crate::smart::parse;

    const DAY: i64 = 24 * 60 * 60;
    const NOW: i64 = 100 * DAY;

    fn disk() -> Disk {
        parse(include_str!("../tests/fixtures/hdd_synthetic.json")).unwrap()
    }

    fn store() -> Store {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        Store { conn }
    }

    fn record(store: &mut Store, disk: &Disk, at: i64) -> i64 {
        let id = store.upsert_disk(disk).unwrap();
        store.save_snapshot(id, disk, &assess(disk, false), false, at).unwrap();
        id
    }

    fn temp_db(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("lastwrite-{}-{name}.sqlite", std::process::id()));
        let _ = fs::remove_file(&path);
        path
    }

    #[test]
    fn open_stamps_the_schema_version_and_keeps_data() {
        let path = temp_db("reopen");
        let first = Store::open(&path).unwrap();
        let id = first.upsert_disk(&disk()).unwrap();
        drop(first);
        let second = Store::open(&path).unwrap();
        assert_eq!(second.known_disks().unwrap()[0].id, id);
        let version: i64 =
            second.conn.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        drop(second);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn open_refuses_a_database_from_a_newer_version() {
        let path = temp_db("newer");
        Connection::open(&path).unwrap().pragma_update(None, "user_version", 99).unwrap();
        let error = Store::open(&path).err().expect("must refuse");
        assert!(error.contains("newer lastwrite"), "{error}");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn no_history_is_not_worsening() {
        let store = store();
        let id = store.upsert_disk(&disk()).unwrap();
        assert!(!store.is_worsening(id, &disk(), NOW).unwrap());
    }

    #[test]
    fn stable_counters_are_not_worsening() {
        let mut store = store();
        let id = record(&mut store, &disk(), NOW - DAY);
        assert!(!store.is_worsening(id, &disk(), NOW).unwrap());
    }

    #[test]
    fn rise_within_window_is_worsening() {
        let mut store = store();
        let id = record(&mut store, &disk(), NOW - 6 * DAY);
        let mut later = disk();
        later.attributes.insert(197, 3);
        assert!(store.is_worsening(id, &later, NOW).unwrap());
    }

    #[test]
    fn rise_since_before_the_window_is_not_worsening() {
        let mut store = store();
        let id = record(&mut store, &disk(), NOW - 8 * DAY);
        let mut later = disk();
        later.attributes.insert(197, 3);
        assert!(!store.is_worsening(id, &later, NOW).unwrap());
    }

    #[test]
    fn non_critical_attribute_rise_is_ignored() {
        let mut store = store();
        let id = record(&mut store, &disk(), NOW - DAY);
        let mut later = disk();
        later.attributes.insert(9, 99999);
        assert!(!store.is_worsening(id, &later, NOW).unwrap());
    }

    #[test]
    fn alert_state_defaults_and_round_trips() {
        let store = store();
        let id = store.upsert_disk(&disk()).unwrap();
        assert_eq!(store.alert_state(id).unwrap(), AlertState::default());
        let state = AlertState { rank: 2, hot: true };
        store.save_alert_state(id, state).unwrap();
        store.save_alert_state(id, state).unwrap();
        assert_eq!(store.alert_state(id).unwrap(), state);
    }

    #[test]
    fn missing_count_rises_and_clears() {
        let store = store();
        let id = store.upsert_disk(&disk()).unwrap();
        assert_eq!(store.mark_missing(id).unwrap(), (1, false));
        assert_eq!(store.mark_missing(id).unwrap(), (2, false));
        store.set_missing_alerted(id).unwrap();
        assert_eq!(store.mark_missing(id).unwrap(), (3, true));
        store.clear_missing(id).unwrap();
        assert_eq!(store.mark_missing(id).unwrap(), (1, false));
    }

    #[test]
    fn missing_count_survives_alert_state_saves() {
        let store = store();
        let id = store.upsert_disk(&disk()).unwrap();
        store.mark_missing(id).unwrap();
        store.save_alert_state(id, AlertState { rank: 1, hot: false }).unwrap();
        assert_eq!(store.mark_missing(id).unwrap(), (2, false));
    }

    #[test]
    fn same_serial_reuses_disk_row() {
        let store = store();
        assert_eq!(store.upsert_disk(&disk()).unwrap(), store.upsert_disk(&disk()).unwrap());
    }

    #[test]
    fn history_returns_the_newest_limit_oldest_first() {
        let mut store = store();
        let id = record(&mut store, &disk(), NOW - 2 * DAY);
        record(&mut store, &disk(), NOW - DAY);
        record(&mut store, &disk(), NOW);
        let history = store.history(id, 2).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].timestamp, NOW - DAY);
        assert_eq!(history[1].timestamp, NOW);
        assert_eq!(history[0].score, Some(79.0));
        assert_eq!(history[0].temperature_c, Some(38));
        assert!(!history[0].worsening);
    }

    #[test]
    fn history_of_unknown_disk_is_empty() {
        let store = store();
        assert!(store.history(999, 10).unwrap().is_empty());
    }
}
