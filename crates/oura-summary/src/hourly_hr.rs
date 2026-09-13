//! Hourly heart-rate candles — the day-shaped view of HR the nightly RHR trend
//! cannot give.
//!
//! The ring never reports "heart rate" as a series: it reports beats. Three decoded
//! streams carry a `hr_bpm` array (see `crates/README.md` in open_oura):
//!
//! * `green_ibi_quality_event` (`0x80`) — daytime, quality-gated pulse estimates,
//! * `ibi_and_amplitude_event` (`0x60`) — overnight beats (6 per 14-byte packet),
//! * `hrv_event` — `interval_min`-minute averages, overnight.
//!
//! Grouping those into local-clock hours gives one bar per hour: the lowest and
//! highest beat actually measured, and the mean of every beat in between. There is no
//! open/close — a heart rate is not a share price, and "the first beat of the hour"
//! carries no information the range and the mean do not. Only `hrv_event` carries
//! per-sample spacing (`interval_min`), so its samples are placed at `ds + i·interval`;
//! the beat streams have no per-sample timestamps and sit at their event's time, which
//! is honest — an event covers seconds, not hours.
//!
//! `latest` deliberately mirrors `build_summary`'s `vitals.hr`: the newest
//! quality-gated green-LED estimate, so the number on the detail screen is the same
//! number as the dashboard cell.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::ring_time::RingClock;
use oura_store::storage::Store;

/// Physiologically plausible range for a wrist/finger PPG beat estimate. The same
/// gate the decoders and `build_summary` apply, kept here so a garbage IBI can never
/// stretch a candle.
const MIN_BPM: f64 = 30.0;
const MAX_BPM: f64 = 240.0;

const HOUR: i64 = 3600;

#[derive(Default)]
struct Bar {
    low: f64,
    high: f64,
    sum: f64,
    count: u64,
}

impl Bar {
    fn add(&mut self, bpm: f64) {
        if self.count == 0 {
            self.low = bpm;
            self.high = bpm;
        } else {
            self.low = self.low.min(bpm);
            self.high = self.high.max(bpm);
        }
        self.sum += bpm;
        self.count += 1;
    }
}

/// One bar per local-clock hour that actually has beats, oldest first.
///
/// `tz` is whole hours from UTC (the same offset `build_summary` takes), `days` caps
/// the window to that many days back from the newest sample — 0 means everything.
///
/// ```text
/// { "tz_offset": 3, "hours": [ { "unix": 1757714400, "ymd": "2026-09-12", "hour": 21,
///                                "low": 48, "high": 71, "mean": 54.2, "count": 812 } ],
///   "latest": { "bpm": 61, "unix": 1757800000 } }
/// ```
pub fn hourly_hr(db: &Path, tz: i64, days: u32) -> Result<Value> {
    let store = Store::open_read_only(db).context("opening DB")?;
    let events = store.decoded_events().context("reading events")?;
    if events.is_empty() {
        return Ok(json!({ "tz_offset": tz, "hours": [], "latest": Value::Null }));
    }
    let clock = RingClock::from_events(&events);

    let mut hours: BTreeMap<i64, Bar> = BTreeMap::new();
    let mut latest: Option<(f64, f64)> = None; // (unix, bpm)

    for (ds, tag, jstr, cu) in &events {
        let name = oura_protocol::events::event_name(*tag);
        if !matches!(
            name,
            "green_ibi_quality_event" | "ibi_and_amplitude_event" | "hrv_event"
        ) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(jstr) else {
            continue;
        };
        let Some(bpms) = v["hr_bpm"].as_array() else {
            continue;
        };
        // hrv_event packs `interval_min`-minute averages; element i is that much later.
        let step_ds = if name == "hrv_event" {
            v["interval_min"].as_i64().unwrap_or(5).max(1) * 600
        } else {
            0
        };
        for (i, x) in bpms.iter().enumerate() {
            let Some(bpm) = x.as_f64() else { continue };
            if !(MIN_BPM..=MAX_BPM).contains(&bpm) {
                continue;
            }
            let at = clock.unix_s(*ds + i as i64 * step_ds, *cu);
            let bucket = bucket_start(at, tz);
            hours.entry(bucket).or_default().add(bpm);
            if name == "green_ibi_quality_event"
                && latest.map_or(true, |(current, _)| at > current)
            {
                latest = Some((at, bpm));
            }
        }
    }

    if days > 0 {
        if let Some(newest) = hours.keys().next_back().copied() {
            let cut = newest - days as i64 * 86_400;
            hours.retain(|start, _| *start >= cut);
        }
    }

    let out: Vec<Value> = hours
        .iter()
        .map(|(start, bar)| {
            let (ymd, hour) = local_ymd_hour(*start, tz);
            json!({
                "unix": start,
                "ymd": ymd,
                "hour": hour,
                "low": bar.low,
                "high": bar.high,
                "mean": (bar.sum / bar.count as f64 * 10.0).round() / 10.0,
                "count": bar.count,
            })
        })
        .collect();

    Ok(json!({
        "tz_offset": tz,
        "hours": out,
        "latest": latest.map(|(at, bpm)| json!({ "bpm": bpm, "unix": at.round() as i64 })),
    }))
}

/// UTC start of the local-clock hour a sample falls in. Bucketing in local time is
/// what makes "3 am" mean 3 am on the wearer's wall clock; the key stays UTC so the
/// chart's x-axis needs no second conversion.
fn bucket_start(unix: f64, tz: i64) -> i64 {
    let local = unix + (tz * HOUR) as f64;
    (local / HOUR as f64).floor() as i64 * HOUR - tz * HOUR
}

/// `(YYYY-MM-DD, hour)` of a bucket start, in the wearer's local clock. Civil-date
/// arithmetic from days-since-epoch (Howard Hinnant's algorithm) — no chrono in the
/// dependency set, and the whole stack works in whole-hour offsets anyway.
fn local_ymd_hour(bucket_start_unix: i64, tz: i64) -> (String, i64) {
    let local = bucket_start_unix + tz * HOUR;
    let days = local.div_euclid(86_400);
    let hour = local.rem_euclid(86_400) / HOUR;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (format!("{y:04}-{m:02}-{d:02}"), hour)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_date_matches_known_instants() {
        // 2026-09-13T00:00:00Z = 1789257600; at tz=+3 that is 03:00 local the same day.
        assert_eq!(
            local_ymd_hour(1_789_257_600, 3),
            ("2026-09-13".to_string(), 3)
        );
        // epoch itself, and a pre-epoch instant (negative days must not panic).
        assert_eq!(local_ymd_hour(0, 0), ("1970-01-01".to_string(), 0));
        assert_eq!(local_ymd_hour(-86_400, 0), ("1969-12-31".to_string(), 0));
    }

    #[test]
    fn buckets_floor_to_the_local_hour() {
        let tz = 3;
        let start = bucket_start(1_789_257_600.0 + 1_900.0, tz);
        assert_eq!(start, 1_789_257_600);
        assert_eq!(bucket_start(1_789_257_600.0 + 3_601.0, tz), 1_789_261_200);
        // a sample before the epoch must floor down, not toward zero
        assert_eq!(bucket_start(-1.0, 0), -3600);
    }

    #[test]
    fn bar_tracks_extremes_and_mean() {
        let mut bar = Bar::default();
        for bpm in [60.0, 80.0, 40.0] {
            bar.add(bpm);
        }
        assert_eq!(bar.low, 40.0);
        assert_eq!(bar.high, 80.0);
        assert_eq!(bar.sum / bar.count as f64, 60.0);
        assert_eq!(bar.count, 3);
    }

    /// End to end over a real store: three sources, three hours, one bar each.
    #[test]
    fn groups_beats_into_local_hours() {
        use oura_protocol::events::RingEvent;

        let dir = std::env::temp_dir().join(format!("oura-hourly-hr-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("oura.db");
        let _ = std::fs::remove_file(&db);

        // Anchor in the recent past: the clock refuses projections far beyond the
        // capture time, and insert_event stamps captured_unix as "now".
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let anchor = (now - 6 * HOUR) / HOUR * HOUR;

        {
            let store = Store::open(db.to_str().unwrap()).unwrap();
            let push = |tag: u8, name: &'static str, ds: i64, decoded: Value| {
                store
                    .insert_event(
                        "S1",
                        &RingEvent {
                            tag,
                            name,
                            timestamp: ds as u32,
                            body: vec![0],
                            decoded: Some(decoded),
                        },
                    )
                    .unwrap();
            };
            push(0x42, "time_sync", 0, json!({ "unix_time": anchor }));
            // +1 h: two daytime beats in one event
            push(
                0x80,
                "green_ibi_quality_event",
                36_000,
                json!({ "hr_bpm": [60, 70] }),
            );
            // +2 h: one beat, and an implausible value the gate must drop
            push(
                0x80,
                "green_ibi_quality_event",
                72_000,
                json!({ "hr_bpm": [50, 500] }),
            );
            // +3 h: overnight 5-minute averages — both land in the same hour
            push(
                0x5d,
                "hrv_event",
                108_000,
                json!({ "interval_min": 5, "hr_bpm": [40, 45] }),
            );
        }

        let v = hourly_hr(&db, 0, 0).unwrap();
        let hours = v["hours"].as_array().unwrap();
        assert_eq!(hours.len(), 3, "{v}");
        assert_eq!(hours[0]["low"], 60.0);
        assert_eq!(hours[0]["high"], 70.0);
        assert_eq!(hours[0]["mean"], 65.0);
        assert_eq!(hours[0]["count"], 2);
        assert_eq!(hours[1]["low"], 50.0, "500 bpm must be gated out: {v}");
        assert_eq!(hours[1]["high"], 50.0);
        assert_eq!(hours[2]["low"], 40.0);
        assert_eq!(hours[2]["high"], 45.0);
        assert_eq!(hours[2]["count"], 2);
        // consecutive hours, one slot apart
        let starts: Vec<i64> = hours.iter().map(|h| h["unix"].as_i64().unwrap()).collect();
        assert_eq!(starts[1] - starts[0], HOUR);
        assert_eq!(starts[2] - starts[1], HOUR);
        // `latest` mirrors the dashboard cell: newest green-LED estimate only
        assert_eq!(v["latest"]["bpm"], 50.0, "{v}");

        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn empty_database_yields_no_hours() {
        let dir = std::env::temp_dir().join(format!("oura-hourly-hr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("oura.db");
        let _ = std::fs::remove_file(&db);
        drop(Store::open(db.to_str().unwrap()).unwrap());
        let v = hourly_hr(&db, 0, 7).unwrap();
        assert_eq!(v["hours"].as_array().unwrap().len(), 0);
        assert!(v["latest"].is_null());
        let _ = std::fs::remove_file(&db);
    }
}
