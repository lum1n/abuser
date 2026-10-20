//! The daily budget: what abuser has spent today and whether it may spend more.
//!
//! Spend is measured, not estimated. While a job runs, every rise in the
//! weekly window's utilisation is charged to abuser ([`Meter`]), including
//! anything you used at the same time. That over-counts on purpose: abuser can
//! then never exceed its limit by under-measuring, and it backs off while
//! you are busy. Charges are dated, so a job that runs past the start of a
//! new budget day charges the new day from then on.
//!
//! The weekly window also sets a pace: by the end of the window's n-th budget
//! day, it may be at most n × `daily_limit_pct` full. Your own use counts
//! here, so abuser only fills what you leave below the pace and can't push
//! the week over it early. Today's budget is used up by whichever of the two
//! comes first ([`used_today`]).
//!
//! Everything here is pure, so the rules can be tested exhaustively.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Local, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{ProviderCfg, ScheduleCfg};
use crate::quota::{Snapshot, Window};

/// A reading older than this can't start a job.
pub const START_FRESHNESS: Duration = Duration::from_secs(10 * 60);

/// Charges rises in the weekly window to the running job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meter {
    /// Highest utilisation already charged (or seen at the start) in the
    /// current window.
    high: f64,
    resets_at: Option<DateTime<Utc>>,
}

impl Meter {
    pub fn start(weekly: &Window) -> Self {
        Self {
            high: weekly.used_pct,
            resets_at: weekly.resets_at,
        }
    }

    /// Takes a new reading and returns the percentage points to charge.
    ///
    /// A reset starts a fresh window: everything in it so far is charged,
    /// since it may all be ours. A reset is the reset time moving on by more
    /// than an hour; without reset times, a fall of more than 10 points.
    /// (Endpoint and harness readings differ by a point or so, which must
    /// not look like a reset.)
    pub fn observe(&mut self, weekly: &Window) -> f64 {
        let reset = match (self.resets_at, weekly.resets_at) {
            (Some(old), Some(new)) => new - old > chrono::Duration::hours(1),
            _ => weekly.used_pct + 10.0 < self.high,
        };
        if reset {
            self.high = 0.0;
        }
        if weekly.resets_at.is_some() {
            self.resets_at = weekly.resets_at;
        }
        let charge = (weekly.used_pct - self.high).max(0.0);
        self.high = self.high.max(weekly.used_pct);
        charge
    }
}

/// Why a job may not start or must stop, for logs and `abuser status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Verdict {
    Go,
    No(String),
}

impl Verdict {
    pub fn ok(&self) -> bool {
        matches!(self, Verdict::Go)
    }
}

fn no(why: impl Into<String>) -> Verdict {
    Verdict::No(why.into())
}

/// The weekly window's length; its start is a week before it resets.
const WEEK: chrono::Duration = chrono::Duration::days(7);

/// How full the weekly window may be at `day_end`: `daily_limit_pct` for
/// each budget day of the window begun by then. `None` without a reset time.
pub fn pace(cfg: &ProviderCfg, weekly: &Window, day_end: DateTime<Utc>) -> Option<f64> {
    let start = weekly.resets_at? - WEEK;
    let secs = (day_end - start).num_seconds().max(0) as f64;
    let days = (secs / 86_400.0).ceil().clamp(1.0, 7.0);
    Some(days * cfg.daily_limit_pct)
}

/// How much of today's budget is used: abuser's own spend `spent`, or more
/// when the weekly window is closer to its pace than that leaves room for.
pub fn used_today(
    cfg: &ProviderCfg,
    spent: f64,
    weekly: Option<&Window>,
    day_end: DateTime<Utc>,
) -> f64 {
    let room = weekly.and_then(|w| Some(pace(cfg, w, day_end)? - w.used_pct));
    match room {
        Some(room) => spent.max(cfg.daily_limit_pct - room),
        None => spent,
    }
}

/// What's left of today's budget, never negative.
pub fn remaining(cfg: &ProviderCfg, used_today: f64) -> f64 {
    (cfg.daily_limit_pct - used_today).max(0.0)
}

/// Whether a new job may start now on this provider.
pub fn can_start(
    cfg: &ProviderCfg,
    schedule: &ScheduleCfg,
    snap: Option<&Snapshot>,
    used_today: f64,
    now: DateTime<Utc>,
) -> Verdict {
    if !cfg.enabled {
        return no("disabled in config");
    }
    if !in_schedule(schedule, now.with_timezone(&Local).time()) {
        return no("outside schedule.windows");
    }
    let Some(snap) = snap else {
        return no("no quota reading");
    };
    if snap.age(now) > START_FRESHNESS {
        return no(format!("quota reading is {}s old", snap.age(now).as_secs()));
    }
    let Some(weekly) = &snap.weekly else {
        return no("no weekly window reported");
    };
    if weekly.used_pct >= cfg.max_weekly_pct {
        return no(format!(
            "weekly window at {:.0}% (max_weekly_pct {:.0}%)",
            weekly.used_pct, cfg.max_weekly_pct
        ));
    }
    if let Some(s) = &snap.session
        && s.used_pct >= cfg.max_session_pct
    {
        return no(format!(
            "5-hour window at {:.0}% (max_session_pct {:.0}%)",
            s.used_pct, cfg.max_session_pct
        ));
    }
    let left = remaining(cfg, used_today);
    if left < cfg.min_job_pct {
        return no(format!(
            "{left:.1}% of today's {:.1}% left (min_job_pct {:.1}%)",
            cfg.daily_limit_pct, cfg.min_job_pct
        ));
    }
    if 100.0 - weekly.used_pct < cfg.min_job_pct {
        return no("weekly window nearly used up");
    }
    Verdict::Go
}

/// Whether a running job must stop now. `snap` is the newest reading,
/// from the endpoint or the harness.
pub fn must_stop(
    cfg: &ProviderCfg,
    snap: Option<&Snapshot>,
    used_today: f64,
    now: DateTime<Utc>,
    stale_after: Duration,
) -> Verdict {
    if used_today >= cfg.daily_limit_pct - cfg.stop_margin_pct {
        return no(format!(
            "today's budget used up: {used_today:.1}% of {:.1}%",
            cfg.daily_limit_pct
        ));
    }
    let Some(snap) = snap else {
        return no("no quota reading");
    };
    if snap.age(now) > stale_after {
        return no(format!(
            "no fresh quota reading for {}s",
            snap.age(now).as_secs()
        ));
    }
    if let Some(w) = &snap.weekly
        && w.used_pct >= cfg.max_weekly_pct
    {
        return no(format!("weekly window at {:.0}%", w.used_pct));
    }
    Verdict::Go
}

pub fn parse_hhmm(s: &str) -> Result<NaiveTime> {
    NaiveTime::parse_from_str(s.trim(), "%H:%M").map_err(|_| anyhow!("expected HH:MM, got {s:?}"))
}

/// `HH:MM-HH:MM`; the end may be before the start (overnight).
pub fn parse_window(s: &str) -> Result<(NaiveTime, NaiveTime)> {
    let Some((a, b)) = s.split_once('-') else {
        bail!("expected HH:MM-HH:MM");
    };
    Ok((parse_hhmm(a)?, parse_hhmm(b)?))
}

pub fn in_schedule(schedule: &ScheduleCfg, t: NaiveTime) -> bool {
    schedule.windows.is_empty()
        || schedule
            .windows
            .iter()
            .filter_map(|w| parse_window(w).ok())
            .any(|(a, b)| {
                if a <= b {
                    a <= t && t < b
                } else {
                    t >= a || t < b
                }
            })
}

/// When the budget day containing `now` started.
pub fn day_start<Tz: TimeZone>(now: DateTime<Tz>, starts: NaiveTime) -> DateTime<Utc> {
    let tz = now.timezone();
    let local = now.naive_local();
    let mut date = local.date();
    if local.time() < starts {
        date = date.pred_opt().unwrap_or(date);
    }
    let naive = date.and_time(starts);
    // On a DST gap, take the earliest valid instant after it.
    tz.from_local_datetime(&naive)
        .earliest()
        .or_else(|| {
            tz.from_local_datetime(&(naive + chrono::Duration::hours(1)))
                .earliest()
        })
        .map_or_else(|| now.with_timezone(&Utc), |t| t.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::{Origin, ProviderId};
    use chrono::{FixedOffset, Timelike};

    fn w(used: f64, reset_h: i64) -> Window {
        Window {
            used_pct: used,
            resets_at: Some(
                Utc.timestamp_opt(1_900_000_000 + reset_h * 3600, 0)
                    .unwrap(),
            ),
        }
    }

    fn snap(weekly: f64, session: f64, age_s: i64, now: DateTime<Utc>) -> Snapshot {
        Snapshot {
            provider: ProviderId::Claude,
            session: Some(Window {
                used_pct: session,
                resets_at: None,
            }),
            weekly: Some(Window {
                used_pct: weekly,
                resets_at: None,
            }),
            plan: None,
            fetched: now - chrono::Duration::seconds(age_s),
            origin: Origin::Endpoint,
        }
    }

    #[test]
    fn meter_charges_rises_only() {
        let mut m = Meter::start(&w(40.0, 0));
        assert_eq!(m.observe(&w(40.0, 0)), 0.0);
        assert_eq!(m.observe(&w(42.0, 0)), 2.0);
        // Jitter down then back up isn't charged twice.
        assert_eq!(m.observe(&w(41.8, 0)), 0.0);
        assert_eq!(m.observe(&w(41.0, 0)), 0.0);
        assert_eq!(m.observe(&w(42.0, 0)), 0.0);
        assert_eq!(m.observe(&w(43.0, 0)), 1.0);
    }

    #[test]
    fn meter_charges_everything_after_a_reset() {
        let mut m = Meter::start(&w(97.0, 0));
        assert_eq!(m.observe(&w(99.0, 0)), 2.0);
        // Window reset: the reset time moved a week on; 3% used since.
        assert_eq!(m.observe(&w(3.0, 168)), 3.0);
        assert_eq!(m.observe(&w(4.0, 168)), 1.0);
        // A drop without a reset time also counts as a reset.
        let mut m = Meter::start(&Window {
            used_pct: 50.0,
            resets_at: None,
        });
        assert_eq!(
            m.observe(&Window {
                used_pct: 2.0,
                resets_at: None
            }),
            2.0
        );
    }

    fn cfg() -> ProviderCfg {
        ProviderCfg {
            daily_limit_pct: 14.0,
            ..ProviderCfg::default()
        }
    }

    #[test]
    fn can_start_table() {
        let now = Utc::now();
        let sched = ScheduleCfg::default();
        let c = cfg();
        let cases: Vec<(&str, Option<Snapshot>, f64, bool)> = vec![
            ("fresh and empty", Some(snap(10.0, 5.0, 10, now)), 0.0, true),
            ("no reading", None, 0.0, false),
            (
                "stale reading",
                Some(snap(10.0, 5.0, 3600, now)),
                0.0,
                false,
            ),
            (
                "weekly too high",
                Some(snap(85.0, 5.0, 10, now)),
                0.0,
                false,
            ),
            (
                "session too high",
                Some(snap(10.0, 60.0, 10, now)),
                0.0,
                false,
            ),
            (
                "budget nearly spent",
                Some(snap(10.0, 5.0, 10, now)),
                12.5,
                false,
            ),
            (
                "just enough left",
                Some(snap(10.0, 5.0, 10, now)),
                12.0,
                true,
            ),
            ("over budget", Some(snap(10.0, 5.0, 10, now)), 20.0, false),
        ];
        for (name, s, spent, want) in cases {
            assert_eq!(
                can_start(&c, &sched, s.as_ref(), spent, now).ok(),
                want,
                "{name}"
            );
        }
        let off = ProviderCfg {
            enabled: false,
            ..cfg()
        };
        assert!(!can_start(&off, &sched, Some(&snap(0.0, 0.0, 0, now)), 0.0, now).ok());
    }

    #[test]
    fn must_stop_table() {
        let now = Utc::now();
        let c = cfg();
        let stale = Duration::from_secs(300);
        let cases: Vec<(&str, Option<Snapshot>, f64, bool)> = vec![
            ("healthy", Some(snap(20.0, 5.0, 30, now)), 5.0, false),
            (
                "at limit minus margin",
                Some(snap(20.0, 5.0, 30, now)),
                13.0,
                true,
            ),
            ("no reading", None, 0.0, true),
            ("stale", Some(snap(20.0, 5.0, 301, now)), 0.0, true),
            ("weekly over max", Some(snap(86.0, 5.0, 30, now)), 0.0, true),
        ];
        for (name, s, spent, want) in cases {
            assert_eq!(
                !must_stop(&c, s.as_ref(), spent, now, stale).ok(),
                want,
                "{name}"
            );
        }
    }

    #[test]
    fn schedule_windows() {
        let s = ScheduleCfg {
            windows: vec!["22:00-07:00".into(), "12:00-13:00".into()],
            ..ScheduleCfg::default()
        };
        let t = |h, m| NaiveTime::from_hms_opt(h, m, 0).unwrap();
        assert!(in_schedule(&s, t(23, 0)));
        assert!(in_schedule(&s, t(3, 0)));
        assert!(in_schedule(&s, t(12, 30)));
        assert!(!in_schedule(&s, t(7, 0)));
        assert!(!in_schedule(&s, t(18, 0)));
        assert!(in_schedule(&ScheduleCfg::default(), t(18, 0)));
    }

    #[test]
    fn day_starts_at_configured_time() {
        let tz = FixedOffset::east_opt(2 * 3600).unwrap();
        let starts = parse_hhmm("04:00").unwrap();
        let before = tz.with_ymd_and_hms(2026, 10, 18, 3, 0, 0).unwrap();
        let after = tz.with_ymd_and_hms(2026, 10, 18, 5, 0, 0).unwrap();
        let d1 = day_start(before, starts).with_timezone(&tz);
        let d2 = day_start(after, starts).with_timezone(&tz);
        assert_eq!((d1.day(), d1.hour()), (17, 4));
        assert_eq!((d2.day(), d2.hour()), (18, 4));
    }

    #[test]
    fn pace_counts_your_use() {
        let c = ProviderCfg {
            daily_limit_pct: 10.0,
            ..ProviderCfg::default()
        };
        let resets = Utc.timestamp_opt(1_900_000_000, 0).unwrap();
        let start = resets - WEEK;
        let weekly = |used| Window {
            used_pct: used,
            resets_at: Some(resets),
        };
        // First day, ending 22 hours into the window: you used 6%.
        let end = start + chrono::Duration::hours(22);
        assert_eq!(pace(&c, &weekly(6.0), end), Some(10.0));
        assert_eq!(used_today(&c, 0.0, Some(&weekly(6.0)), end), 6.0);
        // abuser's own spend counts when it's the larger.
        assert_eq!(used_today(&c, 3.0, Some(&weekly(1.0)), end), 3.0);
        // Third day: quiet earlier days leave room, but no more than the limit.
        let end = start + chrono::Duration::hours(70);
        assert_eq!(pace(&c, &weekly(5.0), end), Some(30.0));
        assert_eq!(used_today(&c, 2.0, Some(&weekly(5.0)), end), 2.0);
        // ...and a busy week leaves none.
        assert_eq!(used_today(&c, 0.0, Some(&weekly(35.0)), end), 15.0);
        // Past the reset the pace is the whole week; without one there's none.
        let late = resets + chrono::Duration::hours(5);
        assert_eq!(pace(&c, &weekly(0.0), late), Some(70.0));
        let none = Window {
            used_pct: 50.0,
            resets_at: None,
        };
        assert_eq!(used_today(&c, 1.0, Some(&none), end), 1.0);
    }

    #[test]
    fn pace_counts_whole_budget_days() {
        let c = ProviderCfg {
            daily_limit_pct: 10.0,
            ..ProviderCfg::default()
        };
        let resets = Utc.timestamp_opt(1_900_000_000, 0).unwrap();
        let start = resets - WEEK;
        let weekly = Window {
            used_pct: 0.0,
            resets_at: Some(resets),
        };
        let h = chrono::Duration::hours;
        let s = chrono::Duration::seconds;
        // A day ending exactly 24 hours in is still the first.
        assert_eq!(pace(&c, &weekly, start + h(24)), Some(10.0));
        assert_eq!(pace(&c, &weekly, start + h(24) + s(1)), Some(20.0));
        // A day ending before the window began counts as its first.
        assert_eq!(pace(&c, &weekly, start - h(30)), Some(10.0));
        assert_eq!(pace(&c, &weekly, start), Some(10.0));
        assert_eq!(pace(&c, &weekly, resets), Some(70.0));
        let unknown = Window {
            used_pct: 0.0,
            resets_at: None,
        };
        assert_eq!(pace(&c, &unknown, resets), None);
        assert_eq!(used_today(&c, 4.0, None, resets), 4.0);
    }

    #[test]
    fn pace_stops_and_holds_jobs() {
        let now = Utc::now();
        let c = cfg();
        let sched = ScheduleCfg::default();
        let s = snap(10.0, 5.0, 10, now);
        let stale = Duration::from_secs(300);
        let resets = Utc.timestamp_opt(1_900_000_000, 0).unwrap();
        // First day of the window, which may be 14% full by its end.
        let end = resets - WEEK + chrono::Duration::hours(20);
        let weekly = |used| Window {
            used_pct: used,
            resets_at: Some(resets),
        };
        // You used 11%: abuser has spent nothing but has only 3% left.
        let used = used_today(&c, 0.0, Some(&weekly(11.0)), end);
        assert_eq!(used, 11.0);
        assert!(can_start(&c, &sched, Some(&s), used, now).ok());
        assert!(must_stop(&c, Some(&s), used, now, stale).ok());
        // At 13% there's too little left to start, and a running job stops.
        let used = used_today(&c, 0.0, Some(&weekly(13.0)), end);
        assert!(!can_start(&c, &sched, Some(&s), used, now).ok());
        assert!(!must_stop(&c, Some(&s), used, now, stale).ok());
        // Past the pace, nothing is left rather than less than nothing.
        let used = used_today(&c, 0.0, Some(&weekly(40.0)), end);
        assert_eq!(used, 40.0);
        assert_eq!(remaining(&c, used), 0.0);
    }

    use chrono::Datelike;
}
