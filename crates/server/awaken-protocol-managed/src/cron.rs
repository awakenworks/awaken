//! A minimal, dependency-free 5-field POSIX cron evaluator for deployment
//! schedules (`BetaManagedAgentsSchedule.expression`): `minute hour day-of-month
//! month day-of-week`. It validates an expression at create/update (a malformed
//! schedule is rejected, not silently stored), matches a wall-clock instant, and
//! computes the next occurrence that drives the timed-trigger firing.
//!
//! Scope: standard fields with `*`, ranges (`a-b`), lists (`a,b`), and steps
//! (`*/n`, `a-b/n`); day-of-week `0`/`7` both mean Sunday; the classic dom∧dow
//! quirk (when BOTH are restricted, a match on EITHER fires). Extended syntax
//! (`L`, `W`, `#`, `?`, `@daily`, seconds/year) is unsupported, matching the SDK.
//! Times are evaluated in UTC; a schedule `timezone` other than UTC is stored and
//! echoed but not yet offset (tz-aware firing is a later step).

use std::collections::BTreeSet;

const MS_PER_MIN: u64 = 60_000;

/// A parsed 5-field cron expression.
#[derive(Debug, Clone)]
pub struct Cron {
    minute: BTreeSet<u32>,
    hour: BTreeSet<u32>,
    dom: BTreeSet<u32>,
    month: BTreeSet<u32>,
    dow: BTreeSet<u32>,
    dom_restricted: bool,
    dow_restricted: bool,
}

impl Cron {
    /// Parse `expr`, returning a human-readable error for a malformed expression.
    pub fn parse(expr: &str) -> Result<Self, String> {
        let f: Vec<&str> = expr.split_whitespace().collect();
        if f.len() != 5 {
            return Err(format!(
                "cron expression must have 5 fields, got {}",
                f.len()
            ));
        }
        let mut dow = parse_field(f[4], 0, 7)?;
        // 7 and 0 both mean Sunday — normalize to 0.
        if dow.remove(&7) {
            dow.insert(0);
        }
        Ok(Self {
            minute: parse_field(f[0], 0, 59)?,
            hour: parse_field(f[1], 0, 23)?,
            dom: parse_field(f[2], 1, 31)?,
            month: parse_field(f[3], 1, 12)?,
            dow,
            dom_restricted: f[2] != "*",
            dow_restricted: f[4] != "*",
        })
    }

    /// True when `ts_ms` (epoch ms, UTC) falls on a scheduled minute.
    pub fn matches(&self, ts_ms: u64) -> bool {
        let (month, day, hour, minute, weekday) = decompose(ts_ms);
        if !self.minute.contains(&minute)
            || !self.hour.contains(&hour)
            || !self.month.contains(&month)
        {
            return false;
        }
        let dom_ok = self.dom.contains(&day);
        let dow_ok = self.dow.contains(&weekday);
        // The POSIX quirk: with BOTH day fields restricted, either matching fires;
        // otherwise the restricted one (the other being `*`) must match.
        if self.dom_restricted && self.dow_restricted {
            dom_ok || dow_ok
        } else {
            dom_ok && dow_ok
        }
    }

    /// The next scheduled instant strictly after `after_ms`, searching up to ~366
    /// days ahead (bounded so an unsatisfiable expression terminates). `None` if
    /// none is found in the window.
    pub fn next_after(&self, after_ms: u64) -> Option<u64> {
        // Start at the next whole minute boundary strictly after `after_ms`.
        let mut minute = after_ms / MS_PER_MIN + 1;
        let limit = minute + 366 * 24 * 60;
        while minute < limit {
            let ts = minute * MS_PER_MIN;
            if self.matches(ts) {
                return Some(ts);
            }
            minute += 1;
        }
        None
    }
}

/// Parse one cron field (`*`, `a`, `a-b`, `a,b`, `*/n`, `a-b/n`) into its allowed
/// value set, bounded to `[min, max]`.
fn parse_field(spec: &str, min: u32, max: u32) -> Result<BTreeSet<u32>, String> {
    let mut out = BTreeSet::new();
    for part in spec.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (
                r,
                s.parse::<u32>()
                    .map_err(|_| format!("bad step in {part:?}"))?,
            ),
            None => (part, 1),
        };
        if step == 0 {
            return Err(format!("step cannot be zero in {part:?}"));
        }
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (
                a.parse::<u32>()
                    .map_err(|_| format!("bad range {part:?}"))?,
                b.parse::<u32>()
                    .map_err(|_| format!("bad range {part:?}"))?,
            )
        } else {
            let v = range
                .parse::<u32>()
                .map_err(|_| format!("bad value {part:?}"))?;
            (v, v)
        };
        if lo < min || hi > max || lo > hi {
            return Err(format!("{part:?} is out of range {min}-{max}"));
        }
        let mut v = lo;
        while v <= hi {
            out.insert(v);
            v += step;
        }
    }
    if out.is_empty() {
        return Err(format!("empty field {spec:?}"));
    }
    Ok(out)
}

/// Format an epoch-ms instant as an RFC 3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`),
/// the shape the schedule wire uses for `scheduled_at` / `last_run_at` / occurrences.
pub fn to_rfc3339(ts_ms: u64) -> String {
    let secs = (ts_ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (h, m, s) = (sod / 3_600, (sod / 60) % 60, sod % 60);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Decompose an epoch-ms instant into `(month, day, hour, minute, weekday)` in UTC,
/// with weekday 0=Sunday..6=Saturday.
fn decompose(ts_ms: u64) -> (u32, u32, u32, u32, u32) {
    let secs = (ts_ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let minute = ((sod / 60) % 60) as u32;
    let hour = (sod / 3_600) as u32;
    // 1970-01-01 was a Thursday (== 4 with Sunday=0).
    let weekday = ((days.rem_euclid(7) + 4).rem_euclid(7)) as u32;
    let (_year, month, day) = civil_from_days(days);
    (month, day, hour, minute, weekday)
}

/// Convert a day count since 1970-01-01 into `(year, month, day)` — Howard
/// Hinnant's `civil_from_days`, exact across the proleptic Gregorian calendar.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (y + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-01-05 09:00:00 UTC is a Monday. (Sanity anchor for the matchers.)
    const MON_0900: u64 = 1_767_603_600_000;

    #[test]
    fn rejects_malformed_expressions() {
        assert!(Cron::parse("* * *").is_err(), "too few fields");
        assert!(Cron::parse("0 9 * * 1-5 2026").is_err(), "too many fields");
        assert!(Cron::parse("60 * * * *").is_err(), "minute out of range");
        assert!(Cron::parse("0 24 * * *").is_err(), "hour out of range");
        assert!(Cron::parse("0 9 * * 8").is_err(), "dow out of range");
        assert!(Cron::parse("0 9 * * */0").is_err(), "zero step");
        assert!(Cron::parse("bad 9 * * *").is_err(), "non-numeric");
    }

    #[test]
    fn accepts_the_documented_examples() {
        assert!(Cron::parse("0 9 * * 1-5").is_ok(), "weekdays at 9am");
        assert!(Cron::parse("*/15 * * * *").is_ok(), "every 15 minutes");
        assert!(Cron::parse("0 0 1 1 *").is_ok(), "new year midnight");
        assert!(Cron::parse("0 9 * * 0").is_ok(), "sunday");
        assert!(Cron::parse("0 9 * * 7").is_ok(), "sunday as 7");
    }

    #[test]
    fn matches_weekday_nine_am() {
        let c = Cron::parse("0 9 * * 1-5").unwrap();
        assert!(c.matches(MON_0900), "monday 9am fires");
        assert!(!c.matches(MON_0900 + 60_000), "9:01 does not");
        assert!(!c.matches(MON_0900 + 5 * 86_400_000), "saturday does not");
    }

    #[test]
    fn dow_zero_and_seven_are_both_sunday() {
        let sunday_0900 = MON_0900 - 86_400_000; // 2026-01-04 is a Sunday
        assert!(Cron::parse("0 9 * * 0").unwrap().matches(sunday_0900));
        assert!(Cron::parse("0 9 * * 7").unwrap().matches(sunday_0900));
    }

    #[test]
    fn both_day_fields_restricted_fire_on_either_match() {
        // The POSIX quirk: `1st of month OR Monday` fires on both, not their AND.
        let c = Cron::parse("0 9 1 * 1").unwrap();
        assert!(c.matches(MON_0900), "a Monday (not the 1st) fires");
        let first_0900 = MON_0900 - 4 * 86_400_000; // 2026-01-01 09:00 (a Thursday)
        assert!(c.matches(first_0900), "the 1st (not a Monday) fires");
        let plain = MON_0900 + 86_400_000; // 2026-01-06, neither the 1st nor Monday
        assert!(!c.matches(plain), "a day that is neither does not fire");
    }

    #[test]
    fn an_unsatisfiable_schedule_yields_no_occurrence() {
        // Feb 30 never exists, so the bounded search returns None rather than
        // looping forever.
        let c = Cron::parse("0 0 30 2 *").unwrap();
        assert!(c.next_after(MON_0900).is_none());
    }

    #[test]
    fn rfc3339_round_trips_the_anchor() {
        assert_eq!(to_rfc3339(MON_0900), "2026-01-05T09:00:00Z");
        assert_eq!(to_rfc3339(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn next_after_advances_to_the_following_occurrence() {
        let c = Cron::parse("*/15 * * * *").unwrap();
        assert_eq!(c.next_after(MON_0900).unwrap(), MON_0900 + 15 * 60_000);
        // Chaining from the returned instant walks the schedule forward.
        let second = c.next_after(MON_0900 + 15 * 60_000).unwrap();
        assert_eq!(second, MON_0900 + 30 * 60_000);
    }
}
