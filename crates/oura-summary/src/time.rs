//! Explicit IANA display timezone (`OURA_TIMEZONE`), validated by chrono-tz.
//!
//! The legacy `build_summary` path takes a fixed UTC-hour offset (`OffsetHours`);
//! per-date DST conversion needs a real zone. Env is the single source: no
//! silent UTC fallback. `Tz::from_str` parses IANA names ("Europe/Paris",
//! "UTC", fixed "+02:00"); anything else is an error naming the variable.

use std::str::FromStr;

/// Read and validate `OURA_TIMEZONE` as an IANA timezone name.
///
/// Missing or empty → `Err` naming the variable (no established default).
/// Unparsable → `Err` naming the variable and the bad value (a timezone name
/// is not a secret; including it makes the fix obvious).
pub fn display_timezone() -> Result<chrono_tz::Tz, String> {
    match std::env::var("OURA_TIMEZONE") {
        Ok(v) if v.trim().is_empty() => Err("OURA_TIMEZONE is set but empty".to_string()),
        Ok(v) => chrono_tz::Tz::from_str(&v)
            .map_err(|_| format!("OURA_TIMEZONE: invalid IANA timezone {v:?}")),
        Err(_) => Err("OURA_TIMEZONE is not set".to_string()),
    }
}

/// Stable IANA name of a zone ("Europe/Paris"); iOS renders it as a label.
pub fn timezone_label(tz: chrono_tz::Tz) -> &'static str {
    tz.name()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Env is process-global: one lock serializes every env-mutating test.
    static ENV: Mutex<()> = Mutex::new(());

    #[test]
    fn env_paths_missing_invalid_and_valid() {
        let _g = ENV.lock().unwrap();
        // Missing → error naming the variable, never a silent UTC fallback.
        std::env::remove_var("OURA_TIMEZONE");
        let err = display_timezone().unwrap_err();
        assert!(err.contains("OURA_TIMEZONE"), "{err}");
        // Empty → same explicit error.
        std::env::set_var("OURA_TIMEZONE", "");
        let err = display_timezone().unwrap_err();
        assert!(err.contains("OURA_TIMEZONE"), "{err}");
        // Invalid value → error naming the variable and the value.
        std::env::set_var("OURA_TIMEZONE", "Mars/Olympus");
        let err = display_timezone().unwrap_err();
        assert!(err.contains("OURA_TIMEZONE") && err.contains("Mars/Olympus"), "{err}");
        // Valid IANA name → parses; the label is the same stable string.
        std::env::set_var("OURA_TIMEZONE", "Europe/Paris");
        let tz = display_timezone().unwrap();
        assert_eq!(timezone_label(tz), "Europe/Paris");
        std::env::remove_var("OURA_TIMEZONE");
    }
}