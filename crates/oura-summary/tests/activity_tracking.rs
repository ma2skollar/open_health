//! Acceptance tests for the `activity_tracking` block (audit tasks A3/B5/B7).
//!
//! Synthetic store fixtures only — no real ring data. Each end-to-end case
//! builds a temp `oura.db` via `oura_store::Store`, inserts a phone
//! `time_sync` (`0x42`) anchor plus `activity_information` (`0x50`) events,
//! then runs `build_summary` and asserts the honest day/hour semantics:
//!
//! - `steps` is always null: true counts need `0x51`/`0x52` decoders (blocked);
//!   `steps_source` is `met_estimate` or `blocked`, never a real count.
//! - DST: local dates from the zone, hourly bins over UTC spans, so 23/25 h
//!   days shrink/grow naturally; no wall duration duplicated or lost.
//! - A day with no valid wear evidence has `sedentary_minutes: null` and
//!   every observed minute unknown — missing is not sedentary.
//! - Boot/counter reuse: a replayed event must not double-count minutes.
//!
//! Direct `build_activity_tracking` cases (no store) pin the same invariants
//! without env coupling.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;

use oura_protocol::events::RingEvent;
use oura_store::storage::Store;

use oura_summary::time::display_timezone;
use oura_summary::{build_activity_tracking, build_summary, Demographics, NoModelRunner};

/// `OURA_TIMEZONE` is process-global and tests run in parallel: one lock
/// serializes every env-touching build so no test reads another's zone.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Ring `activity_information` (0x50) body: state byte + per-minute MET
/// levels, one byte per minute (`b < 0x80 -> b * 0.1` MET).
fn met_body(mets: &[f64]) -> Vec<u8> {
    let mut b = vec![0u8];
    for &m in mets {
        b.push(((m * 10.0).round() as u8).min(0x7f));
    }
    b
}

/// A decoded event straight from `oura_protocol::events::RingEvent`.
fn event(tag: u8, name: &'static str, ds: i64, body: Vec<u8>, decoded: Value) -> RingEvent {
    RingEvent {
        tag,
        name,
        timestamp: ds as u32,
        body,
        decoded: Some(decoded),
    }
}

/// `time_sync` (0x42): the ring counter vs phone wall clock anchor.
fn time_sync(ds: i64, unix: i64) -> RingEvent {
    event(
        0x42,
        "time_sync",
        ds,
        unix.to_le_bytes().to_vec(),
        serde_json::json!({ "unix_time": unix }),
    )
}

/// `activity_information` (0x50) with per-minute MET levels.
fn activity(ds: i64, mets: &[f64]) -> RingEvent {
    let decoded: Vec<Value> = mets
        .iter()
        .map(|m| serde_json::json!((m * 100.0).round() / 100.0))
        .collect();
    event(
        0x50,
        "activity_information",
        ds,
        met_body(mets),
        serde_json::json!({ "state": 0, "met": decoded }),
    )
}

/// Temp dir that removes itself.
struct TempDir(PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!(
            "oura-summary-activity-{tag}-{}-{n}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&p).ok();
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    fn db(&self) -> PathBuf {
        self.0.join("oura.db")
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// `time_sync` anchor pair: ring deciseconds `ds` means UTC epoch `unix`.
/// A whole hour of MET samples starting at that instant.
///
/// Events are inserted with `Store::insert_event`, so `captured_unix` is
/// "now"; anchors must therefore predict the past: `is_dated` needs
/// `predicted <= captured + slack`, which holds when the anchor maps the
/// event's own ring counter onto a wall time <= now.
const ANCHOR_DS: i64 = 10_000_000;
const ANCHOR_UNIX: i64 = 1_767_000_000; // 2025-12-28, well in the past

/// Build a db with the given events (plus one time_sync anchor), then run
/// `build_summary` against it with `OURA_TIMEZONE` set.
fn summary_with(tag: &str, tz: &str, events: &[RingEvent]) -> (TempDir, Value) {
    // Env is process-global: hold the lock from set_var through build so a
    // parallel test can neither read this zone nor clear it mid-build.
    let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    std::env::set_var("OURA_TIMEZONE", tz);
    let dir = TempDir::new(tag);
    let db = dir.db();
    {
        let store = Store::open(&db).unwrap();
        store.insert_event("SERIAL1", &time_sync(ANCHOR_DS, ANCHOR_UNIX)).unwrap();
        for ev in events {
            assert!(store.insert_event("SERIAL1", ev).unwrap(), "inserted {ev:?}");
        }
    }
    let v = build_summary(Path::new(&db), 0.0, &NoModelRunner, &Demographics::default()).unwrap();
    std::env::remove_var("OURA_TIMEZONE");
    (dir, v)
}

/// `days` list from the activity_tracking block.
fn day_list<'a>(v: &'a Value) -> &'a Vec<Value> {
    v["activity_tracking"]["days"].as_array().unwrap()
}

/// Day row by local date.
fn day<'a>(v: &'a Value, date: &str) -> &'a Value {
    day_list(v).iter().find(|d| d["date"] == date).unwrap_or_else(|| {
        panic!("day {date} missing; have {:?}", day_list(v).iter().map(|d| d["date"].as_str()).collect::<Vec<_>>())
    })
}

// ── 1. DST spring-forward: 23 hourly bins ───────────────────────────────────

#[test]
fn dst_spring_forward_day_has_23_hourly_bins() {
    // Europe/Paris 2026-03-29: 02:00 -> 03:00 CET -> CEST; local day = 23 h.
    // 2026-03-29 00:00 CET = 2026-03-28 23:00 UTC = 1_774_751_600.
    let day_start: i64 = 1_774_738_800; // 2026-03-29 00:00 CET = 2026-03-28 23:00 UTC
    let ds = ANCHOR_DS + (day_start - ANCHOR_UNIX) * 10;
    let (dir, v) = summary_with(
        "dst-spring",
        "Europe/Paris",
        &[activity(ds, &[3.0; 60])], // one wall hour of walking MET
    );
    let d = day(&v, "2026-03-29");
    let hours = d["hours"].as_array().unwrap();
    assert_eq!(hours.len(), 23, "spring-forward local day has 23 hourly bins");
    for w in hours.windows(2) {
        let gap = w[1]["start_unix"].as_i64().unwrap() - w[0]["start_unix"].as_i64().unwrap();
        assert_eq!(gap, 3600, "UTC bounds stay 1 h apart across the jump");
    }
    // The single observed hour lands somewhere; totals never double.
    assert_eq!(d["observed_minutes"].as_u64().unwrap(), 60);
    drop(dir);
}

// ── 2. DST fall-back: 25 hourly bins, the 02:00 wall hour twice ────────────────

#[test]
fn dst_fall_back_day_has_25_hourly_bins() {
    // Europe/Paris 2026-10-25: 03:00 -> 02:00 CEST -> CET; local day = 25 h.
    // 2026-10-25 00:00 CEST = 2026-10-24 22:00 UTC = 1_792_879_200.
    let day_start: i64 = 1_792_879_200; // 2026-10-25 00:00 CEST = 2026-10-24 22:00 UTC
    let ds = ANCHOR_DS + (day_start - ANCHOR_UNIX) * 10;
    let (dir, v) = summary_with(
        "dst-fall",
        "Europe/Paris",
        &[activity(ds, &[3.0; 60])],
    );
    let d = day(&v, "2026-10-25");
    let hours = d["hours"].as_array().unwrap();
    assert_eq!(hours.len(), 25, "fall-back local day has 25 hourly bins");
    let labels: Vec<&str> = hours.iter().filter_map(|h| h["label"].as_str()).collect();
    assert_eq!(
        labels.iter().filter(|&&l| l == "02:00").count(),
        2,
        "the repeated 02:00 wall hour appears twice"
    );
    // Distinct UTC bounds for the two 02:00 labels (no overlap).
    let two = hours
        .iter()
        .filter(|h| h["label"] == "02:00")
        .map(|h| h["start_unix"].as_i64().unwrap())
        .collect::<Vec<_>>();
    assert_ne!(two[0], two[1]);
    // The hour before and after the fold map to different UTC offsets:
    // first 02:00 is +02:00, second is +01:00 -> 3600 apart.
    assert_eq!((two[1] - two[0]).abs(), 3600);
    for w in hours.windows(2) {
        let gap = w[1]["start_unix"].as_i64().unwrap() - w[0]["start_unix"].as_i64().unwrap();
        assert_eq!(gap, 3600);
    }
    drop(dir);
}

// ── 3. Fractional-offset zone ────────────────────────────────────────────────

#[test]
fn fractional_offset_zone_pins_local_dates() {
    // 2025-12-30 00:00 +05:45 local = 2025-12-29 18:15 UTC: 18:14 UTC is
    // still 2025-12-29 local, 18:16 UTC is 2025-12-30 (after the anchor).
    let local_midnight_utc: i64 = 1_767_032_100; // 2025-12-30 00:00 +05:45 = 2025-12-29 18:15 UTC
    let ds_a = ANCHOR_DS + (local_midnight_utc - 60 - ANCHOR_UNIX) * 10;
    let ds_b = ANCHOR_DS + (local_midnight_utc + 60 - ANCHOR_UNIX) * 10;
    let (dir, v) = summary_with(
        "fractional",
        "Asia/Kathmandu",
        &[activity(ds_a, &[3.0]), activity(ds_b, &[3.0])],
    );
    let days = day_list(&v);
    assert_eq!(days.len(), 2, "one minute across local midnight splits days");
    let dates: Vec<&str> = days.iter().filter_map(|d| d["date"].as_str()).collect();
    assert!(dates.contains(&"2025-12-29"), "{dates:?}");
    assert!(dates.contains(&"2025-12-30"), "{dates:?}");
    drop(dir);
}

// ── 4. Midnight split in a whole-hour zone ────────────────────────────────────

#[test]
fn midnight_split_puts_adjacent_minutes_on_two_local_dates() {
    // Europe/[ADDRESS] (+01:00): 2025-12-29 23:59 local = 22:59 UTC; the next
    // minute is 2025-12-30 00:00 local on the NEXT local date (after the anchor).
    let t = 1_767_049_140; // 2025-12-29 23:59 local (+01:00) = 22:59 UTC, after the anchor
    let ds_a = ANCHOR_DS + (t - ANCHOR_UNIX) * 10;
    let ds_b = ANCHOR_DS + (t + 60 - ANCHOR_UNIX) * 10;
    let (dir, v) = summary_with(
        "midnight",
        "Europe/Paris",
        &[activity(ds_a, &[3.0]), activity(ds_b, &[3.0])],
    );
    let days = day_list(&v);
    assert_eq!(days.len(), 2);
    let dates: Vec<&str> = days.iter().filter_map(|d| d["date"].as_str()).collect();
    assert!(dates.contains(&"2025-12-29"), "{dates:?}");
    assert!(dates.contains(&"2025-12-30"), "{dates:?}");
    // Both minutes survive: one per day.
    for d in days {
        assert_eq!(d["observed_minutes"].as_u64().unwrap(), 1);
    }
    drop(dir);
}

// ── 5. No-wear-evidence day ──────────────────────────────────────────────────

#[test]
fn no_wear_evidence_day_is_null_sedentary_all_unknown() {
    // Direct: a full day of observed bins with no MET value (None) — the
    // ring was on but reported no activity_information payload.
    let mut met_by_min = std::collections::BTreeMap::new();
    // 2026-11-13 00:00 UTC: any instant; zone decides the date.
    let day0_min = 1_762_992_000_i64 / 60;
    for i in 0..60 {
        met_by_min.insert(day0_min + i, None::<f64>);
    }
    let v = build_activity_tracking(&met_by_min, "Europe/Paris".parse().ok());
    let days = v["days"].as_array().unwrap();
    assert_eq!(days.len(), 1);
    let d = &days[0];
    assert_eq!(d["steps"], Value::Null, "steps always null");
    assert_eq!(d["estimated_steps"], 0.0);
    assert_eq!(d["steps_source"], "blocked");
    assert_eq!(d["observed_minutes"], 60);
    assert_eq!(d["active_minutes"], 0);
    assert_eq!(d["sedentary_minutes"], Value::Null, "no wear evidence -> null, not 0");
    assert_eq!(d["unknown_minutes"], 60, "every observed minute unknown");
    // Invariant: observed = active + sedentary + unknown holds with null sedentary.
    assert_eq!(d["unknown_minutes"].as_u64().unwrap() + 0, 60);
}

#[test]
fn partial_wear_day_splits_minutes() {
    // 30 min sedentary (MET 1.0 < 2.5), 30 min active (MET 3.0): 2026-11-13.
    let mut met_by_min = std::collections::BTreeMap::new();
    // 2026-11-13 00:00 UTC = 1_762_992_000? Verify: 2026-11-13 is the day.
    let base_min = 1_762_992_000_i64 / 60;
    for i in 0..30 {
        met_by_min.insert(base_min + i, Some(1.0));
    }
    for i in 30..60 {
        met_by_min.insert(base_min + i, Some(3.0));
    }
    let v = build_activity_tracking(&met_by_min, "Europe/Paris".parse().ok());
    let days = v["days"].as_array().unwrap();
    assert_eq!(days.len(), 1);
    let d = &days[0];
    assert_eq!(d["observed_minutes"], 60);
    assert_eq!(d["active_minutes"], 30);
    assert_eq!(d["sedentary_minutes"], 30);
    assert_eq!(d["unknown_minutes"], 0);
    // 30 min @ 105/min = 3150 raw -> rounds to 3200? No: 3150/100=31.5 -> 32*100=3200.
    assert_eq!(d["estimated_steps"].as_f64().unwrap(), 3200.0);
    assert_eq!(d["steps_source"], "met_estimate");
}

// ── 6. Boot/counter reuse: replay never duplicates ─────────────────────────────

#[test]
fn boot_counter_reuse_never_duplicates_estimated_steps() {
    // Same events inserted twice (a sync replay re-offering the batch):
    // UNIQUE(serial, tag, ring_timestamp, body) ignores the duplicates, and
    // the minute-bin map dedupes by bin key, so totals are identical.
    let t = ANCHOR_UNIX + 86400; // one day after the anchor, still dated
    let ds = ANCHOR_DS + (t - ANCHOR_UNIX) * 10;
    let events = [activity(ds, &[3.0; 60])];
    let (_d1, v1) = summary_with("boot-a", "Europe/Paris", &events);
    // Replay: same ring counter values, fresh capture time.
    let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    std::env::set_var("OURA_TIMEZONE", "Europe/Paris");
    let dir = TempDir::new("boot-b");
    let db = dir.db();
    {
        let store = Store::open(&db).unwrap();
        store.insert_event("SERIAL1", &time_sync(ANCHOR_DS, ANCHOR_UNIX)).unwrap();
        for ev in &events {
            store.insert_event("SERIAL1", ev).unwrap();
            // Replay: identical (serial, tag, ring_timestamp, body) -> ignored.
            assert!(!store.insert_event("SERIAL1", ev).unwrap(), "dup ignored");
        }
    }
    let v2 = build_summary(Path::new(&db), 0.0, &NoModelRunner, &Demographics::default()).unwrap();
    std::env::remove_var("OURA_TIMEZONE");
    let s1: f64 = day_list(&v1).iter().map(|d| d["estimated_steps"].as_f64().unwrap_or(0.0)).sum();
    let s2: f64 = day_list(&v2).iter().map(|d| d["estimated_steps"].as_f64().unwrap_or(0.0)).sum();
    assert_eq!(s1, s2, "replayed events must not double estimated steps");
    let m1: u64 = day_list(&v1).iter().map(|d| d["observed_minutes"].as_u64().unwrap_or(0)).sum();
    let m2: u64 = day_list(&v2).iter().map(|d| d["observed_minutes"].as_u64().unwrap_or(0)).sum();
    assert_eq!(m1, m2, "replayed events must not double observed minutes");
    drop(dir);
}

// ── 7. Heuristic parity with `activity_daily` ─────────────────────────────────

#[test]
fn estimated_steps_match_legacy_activity_daily_heuristic() {
    // 40 min at MET 3.0 (105/min) + 10 min at MET 8.0 (150/min):
    // raw = 40*105 + 10*150 = 5700 -> (57 round) * 100 = 5700.
    let mut met_by_min = std::collections::BTreeMap::new();
    let base_min = 1_762_992_000_i64 / 60;
    for i in 0..40 {
        met_by_min.insert(base_min + i, Some(3.0));
    }
    for i in 40..50 {
        met_by_min.insert(base_min + i, Some(8.0));
    }
    let v = build_activity_tracking(&met_by_min, "Europe/Paris".parse().ok());
    let d = &v["days"].as_array().unwrap()[0];
    assert_eq!(d["estimated_steps"].as_f64().unwrap(), 5700.0);
    assert_eq!(d["active_minutes"], 50);
    assert_eq!(d["steps"], Value::Null);

    // End-to-end parity: same fixture through build_summary must put the
    // same number in `activity_daily[date].steps` (the legacy field) and in
    // `activity_tracking.days[date].estimated_steps`.
    let ds = ANCHOR_DS + (ANCHOR_UNIX + 3600 - ANCHOR_UNIX) * 10;
    let mut mets = vec![3.0; 40];
    mets.extend_from_slice(&[8.0; 10]);
    let (dir, v) = summary_with("parity", "Europe/Paris", &[activity(ds, &mets)]);
    let at = day_list(&v)
        .iter()
        .find(|d| d["estimated_steps"].as_f64().unwrap_or(0.0) > 0.0)
        .expect("a day with estimated steps");
    let date = at["date"].as_str().unwrap();
    let legacy = v["activity_daily"][date]["steps"].as_f64().expect("legacy steps");
    assert_eq!(at["estimated_steps"].as_f64().unwrap(), legacy, "parity with activity_daily");
    drop(dir);
}

// ── 8. Blocker string + missing-zone degradation ─────────────────────────────

#[test]
fn steps_blocker_string_is_carried_verbatim() {
    let v = build_activity_tracking(&Default::default(), None);
    assert_eq!(
        v["steps_blocker"].as_str().unwrap(),
        "0x51/0x52 counts undecoded upstream; real_steps 0x0b feature off by default and unverified on the target ring"
    );
    assert_eq!(v["days"].as_array().unwrap().len(), 0);
    assert_eq!(v["source"], "summary");
}

#[test]
fn missing_timezone_degrades_to_empty_days() {
    // display_timezone (and the block) must never fabricate UTC dates.
    let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    std::env::remove_var("OURA_TIMEZONE");
    let err = display_timezone().unwrap_err();
    assert!(err.contains("OURA_TIMEZONE"), "{err}");
    let v = build_activity_tracking(&Default::default(), display_timezone().ok());
    assert_eq!(v["days"].as_array().unwrap().len(), 0);
    assert!(v["steps_blocker"].is_string());
}

#[test]
fn timezone_label_is_the_iana_name() {
    let tz = "Pacific/Kiritimati".parse::<chrono_tz::Tz>().unwrap();
    assert_eq!(oura_summary::time::timezone_label(tz), "Pacific/Kiritimati");
}