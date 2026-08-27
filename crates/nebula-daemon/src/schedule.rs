//! Cron parsing and next-fire arithmetic for tasks. Pure — every function
//! takes the current time rather than reading a clock, the same convention
//! `status.rs` uses, so the tests can walk across a DST boundary without
//! waiting for one.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, TimeZone};
use cron::Schedule;
use std::str::FromStr;

/// How late a fire may be and still count. The scheduler ticks every 30s, so
/// a due stamp a few seconds in the past is ordinary lag and must run. A
/// stamp further back than this means nebula wasn't running when the window
/// passed, and that run is skipped rather than fired on startup: coming back
/// from a day offline should not launch a day's worth of agents at once.
pub const MISSED_RUN_GRACE_MS: i64 = 5 * 60 * 1000;

/// Parse a cron expression, accepting both the 5-field form people type
/// (`0 2 * * *` — minute-first, as crontab takes) and the 6-field
/// seconds-first form the `cron` crate wants natively. A 5-field expression
/// is fired on the zeroth second of its minute.
///
/// Note the crate's day-of-week numbering is 1 = Sunday, which is not
/// crontab's 0 = Sunday. Names (`Mon-Fri`) mean the same thing in both and
/// are what the UI suggests; `dow_names_are_stable` pins the behaviour.
pub fn parse(expr: &str) -> Result<Schedule> {
    let expr = expr.trim();
    if expr.is_empty() {
        bail!("a cron expression cannot be empty");
    }
    let normalized = normalize(expr);
    Schedule::from_str(&normalized)
        .with_context(|| format!("`{expr}` is not a cron expression nebula understands"))
}

/// 5 fields → 6 by pinning seconds to zero. Anything else is passed through
/// for the parser to accept or reject on its own terms.
fn normalize(expr: &str) -> String {
    if expr.split_whitespace().count() == 5 {
        format!("0 {expr}")
    } else {
        expr.to_string()
    }
}

/// Whether an expression parses, for validating an edit before it is stored.
pub fn validate(expr: &str) -> Result<()> {
    parse(expr).map(|_| ())
}

/// First fire strictly after `after_ms`, as epoch ms. None when the
/// expression has no future occurrence at all (a pinned year that has
/// passed).
pub fn next_fire_ms(schedule: &Schedule, after_ms: i64) -> Option<i64> {
    let after = from_ms(after_ms)?;
    schedule
        .after(&after)
        .next()
        .map(|dt| dt.timestamp_millis())
}

/// The stamp to store as a task's next due time.
///
/// Normally this is the first fire after `since_ms` — the last run, or the
/// moment the task was defined. When that instant is further behind `now_ms`
/// than [`MISSED_RUN_GRACE_MS`], the window was missed while the daemon was
/// down and is skipped: the answer becomes the first fire after `now_ms`.
/// Returns None when the expression will never fire again.
pub fn next_due_ms(expr: &str, since_ms: i64, now_ms: i64) -> Result<Option<i64>> {
    let schedule = parse(expr)?;
    let from_last = next_fire_ms(&schedule, since_ms);
    Ok(match from_last {
        Some(due) if due >= now_ms - MISSED_RUN_GRACE_MS => Some(due),
        // Missed, or never scheduled from that base: re-anchor on now.
        _ => next_fire_ms(&schedule, now_ms),
    })
}

/// Whether a stored due stamp has come around, allowing for tick lag.
pub fn is_due(next_run_at: i64, now_ms: i64) -> bool {
    next_run_at != 0 && next_run_at <= now_ms
}

fn from_ms(ms: i64) -> Option<DateTime<Local>> {
    Local.timestamp_millis_opt(ms).single()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-03-01T00:00:00Z, well clear of any DST edge in either hemisphere.
    const BASE: i64 = 1_772_323_200_000;
    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;

    #[test]
    fn five_field_crontab_expressions_are_accepted() {
        // The form a user actually types. Without normalization the `cron`
        // crate rejects it outright, which would make every ordinary
        // crontab line an error message.
        assert!(parse("0 2 * * *").is_ok(), "5-field minute-first");
        assert!(parse("*/15 * * * *").is_ok(), "5-field with a step");
        assert!(parse("0 0 2 * * *").is_ok(), "6-field seconds-first");
        assert_eq!(normalize("0 2 * * *"), "0 0 2 * * *");
        assert_eq!(normalize("0 0 2 * * *"), "0 0 2 * * *");
    }

    #[test]
    fn a_bad_expression_is_an_error_not_a_silent_never() {
        // The whole reason create/update validate daemon-side: a typo has to
        // come back to the user, not become a task that never fires.
        for bad in ["", "   ", "not a cron", "0 2 * *", "99 * * * *"] {
            assert!(parse(bad).is_err(), "{bad:?} should not parse");
        }
        assert!(validate("0 2 * * *").is_ok());
    }

    #[test]
    fn next_fire_walks_forward_from_the_given_instant() {
        let s = parse("0 * * * *").unwrap(); // hourly, on the hour
        let first = next_fire_ms(&s, BASE).expect("has a next fire");
        assert!(first > BASE, "must be strictly after the base");
        let second = next_fire_ms(&s, first).expect("and another");
        assert_eq!(second - first, HOUR, "hourly means an hour apart");
    }

    #[test]
    fn a_missed_window_is_skipped_rather_than_fired_on_startup() {
        // Daemon was down for a day: the fire computed from the last run is
        // far behind, so the answer re-anchors on now instead of firing
        // immediately (and then immediately again, and again).
        let last_run = BASE;
        let now = BASE + DAY;
        let due = next_due_ms("0 * * * *", last_run, now)
            .unwrap()
            .expect("hourly always has a next fire");
        assert!(
            due > now,
            "a day-old window should be skipped, got {} vs now {}",
            due,
            now
        );
    }

    #[test]
    fn a_fire_inside_the_grace_window_still_runs() {
        // Ordinary tick lag: the scheduler wakes 30s after the stamp. That
        // run is not missed and must not be skipped forward.
        let last_run = BASE;
        let s = parse("0 * * * *").unwrap();
        let due_stamp = next_fire_ms(&s, last_run).unwrap();
        let now = due_stamp + 30_000;
        let due = next_due_ms("0 * * * *", last_run, now).unwrap().unwrap();
        assert_eq!(due, due_stamp, "a 30s-late fire is still this fire");
        assert!(is_due(due, now), "and it reads as due");
    }

    #[test]
    fn is_due_treats_zero_as_not_scheduled() {
        // 0 is the "no cron, or disabled" stamp, and must never look due
        // however far the clock has advanced.
        assert!(!is_due(0, BASE));
        assert!(is_due(BASE, BASE), "exactly due counts");
        assert!(!is_due(BASE + MINUTE, BASE));
    }

    /// Local-time arithmetic is the entire reason `chrono` is a dependency:
    /// "02:00" has to stay 02:00 wall-clock across a DST transition, which a
    /// fixed 86_400_000ms step cannot do. This asserts the property that
    /// matters — consecutive daily fires land on the same wall-clock hour —
    /// rather than a specific offset, so it holds in any timezone CI uses.
    #[test]
    fn a_daily_fire_keeps_its_wall_clock_hour_across_a_dst_change() {
        use chrono::Timelike;
        let s = parse("0 2 * * *").unwrap();
        // Start a week before the US spring-forward (2027-03-14) and walk
        // through it; in a no-DST zone this simply passes trivially.
        let start = Local
            .with_ymd_and_hms(2027, 3, 8, 12, 0, 0)
            .single()
            .expect("unambiguous noon");
        let mut at = start;
        for step in 0..10 {
            at = s.after(&at).next().expect("daily always continues");
            assert_eq!(at.hour(), 2, "step {step} drifted off 02:00: {at}");
            assert_eq!(at.minute(), 0, "step {step} drifted off the hour: {at}");
        }
    }

    /// Day-of-week names are the form the UI suggests precisely because they
    /// survive the crate's 1 = Sunday numbering. This documents that
    /// `Mon-Fri` really is Monday through Friday.
    #[test]
    fn dow_names_are_stable() {
        use chrono::{Datelike, Weekday};
        let s = parse("0 9 * * Mon-Fri").unwrap();
        let start = Local
            .with_ymd_and_hms(2026, 3, 1, 0, 0, 0)
            .single()
            .expect("unambiguous midnight");
        let mut at = start;
        for _ in 0..10 {
            at = s.after(&at).next().expect("weekdays continue");
            assert!(
                !matches!(at.weekday(), Weekday::Sat | Weekday::Sun),
                "Mon-Fri fired on a weekend: {at}"
            );
        }
    }
}
