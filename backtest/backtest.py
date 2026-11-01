"""Backtest the TRD's HDD scoring rules against Backblaze drive data.

Usage: backtest/.venv/bin/python -I backtest/backtest.py

Answers: when a rule flags a drive, how often does it really fail soon, and
how many failures does it catch 30-60 days ahead?
"""
import csv
import os

import duckdb

HERE = os.path.dirname(os.path.abspath(__file__))
CSV_GLOB = os.path.join(HERE, "data/csv/*.csv")
DAILY = os.path.join(HERE, "data/daily.parquet")
RESULTS = os.path.join(HERE, "results.csv")
RESULTS_BANDS = os.path.join(HERE, "results_bands.csv")

HORIZON_DAYS = 60
SNAPSHOT_DATES = ("2024-03-01", "2024-05-01", "2024-07-01", "2024-09-01")

# The TRD gives ranges, not exact steps. These are this script's reading of it.
PENDING_PENALTY = 5        # per sector, IDs 197 and 198
SPINDLE_PENALTY = 10       # flat, if ID 10 > 0
REALLOC_TIERS = (10, 100)  # -1 per sector up to 10, -0.5 up to 100, -0.2 after
VELOCITY_WINDOW_DAYS = 7
VELOCITY_BIG_DELTA = 10    # below: 1.5x, at or above: 2x

HEALTH_CUTOFFS = (95, 90, 80, 60, 40, 20)


def prepare(con):
    if os.path.exists(DAILY):
        return
    con.execute(f"""
        COPY (
            SELECT CAST(date AS DATE) AS d, serial_number AS serial, model,
                   TRY_CAST(failure AS INT) AS failure,
                   coalesce(TRY_CAST(smart_5_raw AS BIGINT), 0) AS s5,
                   coalesce(TRY_CAST(smart_10_raw AS BIGINT), 0) AS s10,
                   coalesce(TRY_CAST(smart_197_raw AS BIGINT), 0) AS s197,
                   coalesce(TRY_CAST(smart_198_raw AS BIGINT), 0) AS s198
            FROM read_csv('{CSV_GLOB}', header=true, all_varchar=true,
                          union_by_name=true)
            WHERE {hdd_filter()}
        ) TO '{DAILY}' (FORMAT parquet)
    """)


def hdd_filter():
    # Spin_Up_Time only exists on spinning drives, and the SSD models say so.
    return """
        TRY_CAST(smart_3_raw AS BIGINT) IS NOT NULL
        AND upper(model) NOT LIKE '%SSD%'
    """


def build_tables(con):
    con.execute(f"""
        CREATE TABLE drives AS
        SELECT serial, any_value(model) AS model,
               min(d) AS first_seen, max(d) AS last_seen,
               min(CASE WHEN failure = 1 THEN d END) AS fail_date,
               max(s5 + s10 + s197 + s198) > 0 AS has_signal,
               CASE WHEN substr(md5(serial), 1, 1) < '8'
                    THEN 'calibrate' ELSE 'holdout' END AS split
        FROM '{DAILY}' GROUP BY serial
    """)

    low, high = REALLOC_TIERS
    realloc = f"""
        least(s5, {low}) * 1.0
        + greatest(least(s5, {high}) - {low}, 0) * 0.5
        + greatest(s5 - {high}, 0) * 0.2
    """
    # Only drives with a non-zero attribute can ever score below 100.
    window = f"""OVER (PARTITION BY serial ORDER BY d RANGE BETWEEN
                 INTERVAL {VELOCITY_WINDOW_DAYS} DAYS PRECEDING AND CURRENT ROW)"""
    con.execute(f"""
        CREATE TABLE scored AS
        WITH base AS (
            SELECT t.d, t.serial, s5, s197, s198,
                   ({PENDING_PENALTY} * (s197 + s198) + {realloc}
                    + CASE WHEN s10 > 0 THEN {SPINDLE_PENALTY} ELSE 0 END) AS penalty,
                   (s5 - min(s5) {window}) + (s197 - min(s197) {window})
                   + (s198 - min(s198) {window}) + (s10 - min(s10) {window}) AS delta
            FROM '{DAILY}' t JOIN drives USING (serial)
            WHERE has_signal AND (fail_date IS NULL OR t.d <= fail_date)
        )
        SELECT d, serial, s5, s197, s198, delta,
               greatest(100 - penalty, 0) AS health_static,
               greatest(100 - penalty * CASE
                   WHEN delta >= {VELOCITY_BIG_DELTA} THEN 2.0
                   WHEN delta > 0 THEN 1.5 ELSE 1.0 END, 0) AS health_velocity
        FROM base
    """)


def rules():
    found = {"any_sector_signal": "s5 + s197 + s198 > 0",
             "velocity_only": "delta > 0"}
    for cutoff in HEALTH_CUTOFFS:
        found[f"static_lt_{cutoff}"] = f"health_static < {cutoff}"
        found[f"velocity_lt_{cutoff}"] = f"health_velocity < {cutoff}"
    return found


def base_rate(con, split):
    """Chance a random running drive fails in the next HORIZON_DAYS."""
    return con.execute(f"""
        SELECT avg(CASE WHEN fail_date <= DATE '2024-07-01' + {HORIZON_DAYS}
                   THEN 1.0 ELSE 0.0 END)
        FROM drives
        WHERE split = '{split}' AND first_seen <= DATE '2024-07-01'
          AND last_seen >= DATE '2024-07-01'
          AND (fail_date IS NULL OR fail_date > DATE '2024-07-01')
    """).fetchone()[0]


def evaluate(con, rule, condition, split, rate):
    con.execute(f"""
        CREATE OR REPLACE TEMP TABLE flags AS
        SELECT serial, d FROM scored JOIN drives USING (serial)
        WHERE split = '{split}' AND {condition}
    """)
    caught = con.execute(f"""
        SELECT count(*),
               count(*) FILTER (WHERE zero_signal),
               count(*) FILTER (WHERE early),
               count(*) FILTER (WHERE late)
        FROM (
            SELECT f.serial, NOT f.has_signal AS zero_signal,
                   coalesce(bool_or(x.d BETWEEN f.fail_date - 60
                                    AND f.fail_date - 30), false) AS early,
                   coalesce(bool_or(x.d BETWEEN f.fail_date - 60
                                    AND f.fail_date - 1), false) AS late
            FROM drives f LEFT JOIN flags x USING (serial)
            WHERE f.split = '{split}' AND f.fail_date IS NOT NULL
              AND f.first_seen <= f.fail_date - 60
            GROUP BY ALL
        )
    """).fetchone()
    last_day = con.execute("SELECT max(last_seen) FROM drives").fetchone()[0]
    # Skip drives already flagged on day one: we can't see when that started.
    outcome = con.execute(f"""
        WITH firsts AS (SELECT serial, min(d) AS flagged FROM flags GROUP BY 1)
        SELECT count(*),
               count(*) FILTER (WHERE fail_date = flagged),
               count(*) FILTER (WHERE fail_date BETWEEN flagged + 1 AND flagged + 7),
               count(*) FILTER (WHERE fail_date BETWEEN flagged + 8 AND flagged + 30),
               count(*) FILTER (WHERE fail_date BETWEEN flagged + 31
                                AND flagged + {HORIZON_DAYS}),
               count(*) FILTER (WHERE fail_date IS NULL
                                AND last_seen < flagged + {HORIZON_DAYS})
        FROM firsts JOIN drives USING (serial)
        WHERE flagged > first_seen
          AND flagged <= DATE '{last_day}' - {HORIZON_DAYS}
    """).fetchone()
    population = con.execute(f"""
        SELECT count(*) FROM drives WHERE split = '{split}'
    """).fetchone()[0]
    # What a user actually sees: every drive flagged today, however old the flag.
    dates = ", ".join(f"DATE '{d}'" for d in SNAPSHOT_DATES)
    standing, standing_failed = con.execute(f"""
        SELECT count(*), count(*) FILTER (WHERE fail_date <= x.d + {HORIZON_DAYS})
        FROM flags x JOIN drives USING (serial)
        WHERE x.d IN ({dates}) AND last_seen >= x.d
          AND (fail_date IS NULL OR fail_date > x.d)
    """).fetchone()
    fails, zero, early, late = caught
    flagged, same_day, week, month, two_months, removed = outcome
    failed = week + month + two_months
    return {
        "rule": rule, "split": split, "failed_drives": fails,
        "no_signal_ever_pct": pct(zero, fails),
        "caught_30_60d_pct": pct(early, fails),
        "caught_1_60d_pct": pct(late, fails),
        "new_flags": flagged,
        "flags_per_1000_drives": round(1000 * flagged / population, 1),
        "same_day_fail_pct": pct(same_day, flagged),
        "fail_1_7d_pct": pct(week, flagged),
        "fail_8_30d_pct": pct(month, flagged),
        "fail_31_60d_pct": pct(two_months, flagged),
        "failed_within_60d_pct": pct(failed, flagged),
        "removed_unknown_pct": pct(removed, flagged),
        "lift_over_base_rate": round(failed / flagged / rate, 1) if flagged else 0,
        "standing_flags": standing,
        "standing_failed_60d_pct": pct(standing_failed, standing),
    }


def band_report(con):
    """Failure rate of running drives by health band, sampled on SNAPSHOT_DATES."""
    dates = ", ".join(f"(DATE '{d}')" for d in SNAPSHOT_DATES)
    rows = con.execute(f"""
        WITH snap AS (
            SELECT dates.snap_d, dr.split, dr.fail_date,
                   CASE WHEN dr.model LIKE 'ST%' OR dr.model LIKE 'Seagate%' THEN 'Seagate'
                        WHEN dr.model LIKE 'TOSHIBA%' OR dr.model LIKE 'MG%' THEN 'Toshiba'
                        WHEN dr.model LIKE 'HGST%' OR dr.model LIKE 'WUH%' THEN 'HGST'
                        WHEN dr.model LIKE 'WD%' THEN 'WDC'
                        ELSE 'other' END AS maker,
                   coalesce(s.health_static, 100) AS health,
                   coalesce(s.delta, 0) > 0 AS worsening
            FROM (VALUES {dates}) AS dates(snap_d)
            CROSS JOIN drives dr
            LEFT JOIN scored s ON s.serial = dr.serial AND s.d = dates.snap_d
            WHERE dr.first_seen <= dates.snap_d AND dr.last_seen >= dates.snap_d
              AND (dr.fail_date IS NULL OR dr.fail_date > dates.snap_d)
        ), banded AS (
            SELECT *, CASE WHEN health >= 100 THEN '100'
                           WHEN health >= 90 THEN '90-99'
                           WHEN health >= 60 THEN '60-89'
                           WHEN health >= 20 THEN '20-59'
                           ELSE '0-19' END AS band
            FROM snap
        )
        SELECT split, maker, band, worsening, count(*) AS drive_days,
               count(*) FILTER (WHERE fail_date <= snap_d + 30) AS failed_30d,
               count(*) FILTER (WHERE fail_date <= snap_d + {HORIZON_DAYS}) AS failed_60d
        FROM banded GROUP BY ALL ORDER BY ALL
    """).fetchall()
    with open(RESULTS_BANDS, "w", newline="") as out:
        writer = csv.writer(out)
        writer.writerow(["split", "maker", "band", "worsening", "drive_days",
                         "failed_30d", "failed_60d"])
        writer.writerows(rows)


def pct(part, whole):
    return round(100 * part / whole, 1) if whole else 0


def main():
    con = duckdb.connect()
    con.execute("SET temp_directory = '" + os.path.join(HERE, "data/tmp") + "'")
    prepare(con)
    build_tables(con)
    rows = []
    for split in ("calibrate", "holdout"):
        rate = base_rate(con, split)
        print(f"{split}: {HORIZON_DAYS}-day base failure rate {100 * rate:.3f}%")
        for rule, condition in rules().items():
            rows.append(evaluate(con, rule, condition, split, rate))
    with open(RESULTS, "w", newline="") as out:
        writer = csv.DictWriter(out, fieldnames=rows[0].keys())
        writer.writeheader()
        writer.writerows(rows)
    band_report(con)
    print(f"wrote {RESULTS} and {RESULTS_BANDS}")


if __name__ == "__main__":
    main()
