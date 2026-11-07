//! Wake-window scheduling uses UTC instants and local IANA wall time. This
//! naturally handles repeated and missing DST minutes without inventing times.

use chrono::{DateTime, NaiveTime, Utc};
use chrono_tz::Tz;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::config::parse_wake_time;
use crate::storage::{backoff, open_private_database};
use crate::{CollectorError, Config};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SchedulerState {
    pub last_run: Option<i64>,
    pub next_run: i64,
    pub consecutive_failures: u32,
    pub cursor: u64,
}

pub struct Scheduler {
    pub state: SchedulerState,
    connection: Connection,
    collector_id: String,
    epoch: String,
    timezone: Tz,
    wake_start: NaiveTime,
    wake_end: NaiveTime,
    retry_min_secs: u64,
    retry_max_secs: u64,
    max_attempts: u32,
}

impl Scheduler {
    pub fn open(config: &Config) -> Result<Self, CollectorError> {
        config.prepare_state()?;
        let connection = open_private_database(&config.state_dir.join("scheduler.sqlite"))?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS scheduler(
                collector_id TEXT NOT NULL,epoch TEXT NOT NULL,state TEXT NOT NULL,
                PRIMARY KEY(collector_id,epoch));")?;
        let encoded: Option<String> = connection.query_row(
            "SELECT state FROM scheduler WHERE collector_id=?1 AND epoch=?2",
            params![config.collector_id, config.epoch], |row| row.get(0),
        ).optional()?;
        let state = match encoded {
            Some(encoded) => serde_json::from_str(&encoded)?,
            None => SchedulerState { last_run: None, next_run: 0, consecutive_failures: 0, cursor: 0 },
        };
        let scheduler = Self {
            state, connection, collector_id: config.collector_id.clone(), epoch: config.epoch.clone(),
            timezone: config.timezone.parse().map_err(|_| CollectorError::Config("invalid IANA timezone".into()))?,
            wake_start: parse_wake_time(&config.wake_start)?, wake_end: parse_wake_time(&config.wake_end)?,
            retry_min_secs: config.retry_min_secs, retry_max_secs: config.retry_max_secs,
            max_attempts: config.max_attempts,
        };
        scheduler.persist(&scheduler.state)?;
        Ok(scheduler)
    }

    pub fn due(&self, now: i64) -> bool { now >= self.state.next_run && self.in_window(now) }

    pub fn success(&mut self, now: i64, cursor: u64) -> Result<(), CollectorError> {
        let state = SchedulerState {
            last_run: Some(now),
            // Without a separate poll interval, the configured maximum backoff is
            // also the normal collection interval. Never continuously scan.
            next_run: self.next_allowed(now.saturating_add(self.retry_max_secs as i64))?,
            consecutive_failures: 0, cursor,
        };
        self.persist(&state)?;
        self.state = state;
        Ok(())
    }

    /// Persist snapshot recovery progress without recording a ring attempt.
    pub fn checkpoint_cursor(&mut self, cursor: u64) -> Result<(), CollectorError> {
        let state = SchedulerState { cursor, ..self.state.clone() };
        self.persist(&state)?;
        self.state = state;
        Ok(())
    }

    pub fn failure(&mut self, now: i64) -> Result<(), CollectorError> {
        let failures = self.state.consecutive_failures.saturating_add(1);
        let attempt = (failures.saturating_sub(1) % self.max_attempts) + 1;
        let earliest = now.saturating_add(backoff(self.retry_min_secs, self.retry_max_secs, attempt) as i64);
        let next_run = if attempt == self.max_attempts {
            // Spend at most max_attempts per wake window, then sleep until a
            // later window, even when the ring remains absent all day.
            self.next_window_after(now, earliest)?
        } else { self.next_allowed(earliest)? };
        let state = SchedulerState { last_run: Some(now), next_run, consecutive_failures: failures, cursor: self.state.cursor };
        self.persist(&state)?;
        self.state = state;
        Ok(())
    }

    fn in_window(&self, unix: i64) -> bool {
        let Some(utc) = DateTime::<Utc>::from_timestamp(unix, 0) else { return false };
        let local = utc.with_timezone(&self.timezone).time();
        if self.wake_start < self.wake_end { local >= self.wake_start && local < self.wake_end }
        else { local >= self.wake_start || local < self.wake_end }
    }

    fn next_allowed(&self, earliest: i64) -> Result<i64, CollectorError> {
        if self.in_window(earliest) { return Ok(earliest); }
        let mut candidate = earliest.checked_add(60 - earliest.rem_euclid(60))
            .ok_or_else(|| CollectorError::Config("scheduler timestamp overflow".into()))?;
        // Three UTC days cover a skipped civil day as well as DST transitions.
        for _ in 0..(3 * 24 * 60) {
            if self.in_window(candidate) { return Ok(candidate); }
            candidate = candidate.checked_add(60).ok_or_else(|| CollectorError::Config("scheduler timestamp overflow".into()))?;
        }
        Err(CollectorError::Config("no wake window found within three days".into()))
    }

    fn next_window_after(&self, now: i64, earliest: i64) -> Result<i64, CollectorError> {
        let mut candidate = now.checked_add(60 - now.rem_euclid(60))
            .ok_or_else(|| CollectorError::Config("scheduler timestamp overflow".into()))?;
        let mut left_current = !self.in_window(now);
        for _ in 0..(3 * 24 * 60) {
            let inside = self.in_window(candidate);
            if !inside { left_current = true; }
            if left_current && inside && candidate >= earliest { return Ok(candidate); }
            candidate = candidate.checked_add(60).ok_or_else(|| CollectorError::Config("scheduler timestamp overflow".into()))?;
        }
        Err(CollectorError::Config("no later wake window found within three days".into()))
    }

    fn persist(&self, state: &SchedulerState) -> Result<(), CollectorError> {
        self.connection.execute("INSERT INTO scheduler(collector_id,epoch,state) VALUES(?1,?2,?3) ON CONFLICT(collector_id,epoch) DO UPDATE SET state=excluded.state",
            params![self.collector_id, self.epoch, serde_json::to_string(state)?])?;
        Ok(())
    }
}
