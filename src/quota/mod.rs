//! How much of a subscription is used, per rate-limit window.
//!
//! Two signals feed it: the providers' usage endpoints (this module), polled
//! while idle and before every job, and the `Quota` events a running harness
//! reports through acc (see [`from_acc`]). Both endpoints are private and
//! undocumented, so each provider stays in its own file and parses leniently.

mod claude;
mod codex;

use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use claude::Claude;
pub use codex::Codex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderId {
    Claude,
    Codex,
}

impl ProviderId {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub fn harness(self) -> acc_core::Harness {
        match self {
            Self::Claude => acc_core::Harness::Claude,
            Self::Codex => acc_core::Harness::Codex,
        }
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One rate-limit window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// 0–100.
    pub used_pct: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// The provider's usage endpoint.
    Endpoint,
    /// A `Quota` event from a running harness.
    Harness,
}

/// A provider's windows at one moment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub provider: ProviderId,
    /// The short (5-hour) window.
    pub session: Option<Window>,
    /// The weekly window, which the daily budget is a share of.
    pub weekly: Option<Window>,
    pub plan: Option<String>,
    pub fetched: DateTime<Utc>,
    pub origin: Origin,
}

impl Snapshot {
    pub fn age(&self, now: DateTime<Utc>) -> Duration {
        (now - self.fetched).to_std().unwrap_or_default()
    }

    /// This snapshot with windows it lacks taken from `older`, for harness
    /// events that report only the window currently in force.
    pub fn merged_over(mut self, older: &Snapshot) -> Snapshot {
        if self.session.is_none() {
            self.session = older.session.clone();
        }
        if self.weekly.is_none() {
            self.weekly = older.weekly.clone();
        }
        if self.plan.is_none() {
            self.plan = older.plan.clone();
        }
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum QuotaError {
    #[error("not signed in: {0}")]
    NoCredentials(String),
    #[error("credentials rejected (sign in again with the CLI)")]
    Unauthorized,
    #[error("rate limited by the usage endpoint; retry in {0:?}")]
    RateLimited(Duration),
    #[error("usage endpoint: {0}")]
    Http(String),
    #[error("unreadable usage response: {0}")]
    Parse(String),
}

#[async_trait::async_trait]
pub trait QuotaProvider: Send + Sync {
    /// The current windows. Implementations cache and back off themselves,
    /// so calling this often is fine.
    async fn usage(&self) -> Result<Snapshot, QuotaError>;
}

pub fn provider(id: ProviderId, cfg: &crate::config::ProviderCfg) -> Box<dyn QuotaProvider> {
    match id {
        ProviderId::Claude => Box::new(Claude::new(cfg.usage_url.clone(), cfg.min_fetch_secs)),
        ProviderId::Codex => Box::new(Codex::new(cfg.usage_url.clone(), cfg.min_fetch_secs)),
    }
}

/// A snapshot from a harness's `Quota` event, if it names a known window.
pub fn from_acc(
    provider: ProviderId,
    q: &acc_core::event::Quota,
    now: DateTime<Utc>,
) -> Option<Snapshot> {
    let mut session = None;
    let mut weekly = None;
    for w in &q.windows {
        let Some(used) = w.used_percent else { continue };
        let win = Window {
            used_pct: clamp_pct(used),
            resets_at: w
                .resets_at
                .and_then(|s| Utc.timestamp_opt(s as i64, 0).single()),
        };
        let mins = w.window_minutes.unwrap_or(0);
        match w.id.as_str() {
            "five_hour" | "primary" => session = Some(win),
            "seven_day" | "secondary" => weekly = Some(win),
            _ if mins == 300 => session = Some(win),
            _ if mins == 10_080 => weekly = Some(win),
            // Model-scoped weekly windows (`seven_day_opus`) don't count
            // toward the overall weekly budget.
            _ => {}
        }
    }
    (session.is_some() || weekly.is_some()).then(|| Snapshot {
        provider,
        session,
        weekly,
        plan: q.plan.clone(),
        fetched: now,
        origin: Origin::Harness,
    })
}

pub(crate) fn clamp_pct(v: f64) -> f64 {
    if v.is_finite() {
        v.clamp(0.0, 100.0)
    } else {
        100.0
    }
}

/// A reset time as RFC 3339 text or epoch seconds.
pub(crate) fn parse_time(v: Option<&Value>) -> Option<DateTime<Utc>> {
    match v? {
        Value::String(s) => DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.with_timezone(&Utc)),
        Value::Number(n) => {
            let secs = n.as_f64()?;
            // Milliseconds slip in on some responses.
            let secs = if secs > 1e11 { secs / 1000.0 } else { secs };
            Utc.timestamp_opt(secs as i64, 0).single()
        }
        _ => None,
    }
}

pub(crate) fn first<'a>(obj: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .find_map(|k| obj.get(*k).filter(|v| !v.is_null()))
}

/// Shared fetch pacing: a minimum interval between requests, and
/// exponential backoff after a 429.
#[derive(Debug)]
pub(crate) struct Pacer {
    interval: Duration,
    last: Option<(std::time::Instant, Snapshot)>,
    blocked_until: Option<std::time::Instant>,
    backoff: Duration,
}

const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);

impl Pacer {
    pub fn new(min_fetch_secs: u64) -> Self {
        Self {
            interval: Duration::from_secs(min_fetch_secs),
            last: None,
            blocked_until: None,
            backoff: Duration::ZERO,
        }
    }

    /// The cached snapshot when a fetch now would be too soon.
    pub fn cached(&self) -> Option<Result<Snapshot, QuotaError>> {
        let now = std::time::Instant::now();
        if let Some(until) = self.blocked_until
            && now < until
        {
            return Some(Err(QuotaError::RateLimited(until - now)));
        }
        match &self.last {
            Some((at, snap)) if now.duration_since(*at) < self.interval => Some(Ok(snap.clone())),
            _ => None,
        }
    }

    pub fn ok(&mut self, snap: &Snapshot) {
        self.last = Some((std::time::Instant::now(), snap.clone()));
        self.blocked_until = None;
        self.backoff = Duration::ZERO;
    }

    pub fn rate_limited(&mut self, retry_after: Option<Duration>) -> Duration {
        self.backoff = if self.backoff.is_zero() {
            Duration::from_secs(60)
        } else {
            (self.backoff * 2).min(MAX_BACKOFF)
        };
        let wait = retry_after.unwrap_or(self.backoff).min(MAX_BACKOFF);
        self.blocked_until = Some(std::time::Instant::now() + wait);
        wait
    }
}

pub(crate) fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

pub(crate) fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("subr/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use acc_core::event::{Quota, QuotaWindow};

    #[test]
    fn acc_quota_maps_known_windows() {
        let q = Quota {
            account: None,
            plan: Some("max".into()),
            windows: vec![
                QuotaWindow {
                    id: "five_hour".into(),
                    used_percent: Some(12.0),
                    window_minutes: Some(300),
                    resets_at: Some(1_900_000_000),
                    status: None,
                },
                QuotaWindow {
                    id: "seven_day_opus".into(),
                    used_percent: Some(90.0),
                    ..Default::default()
                },
            ],
        };
        let s = from_acc(ProviderId::Claude, &q, Utc::now()).unwrap();
        assert_eq!(s.session.unwrap().used_pct, 12.0);
        assert!(
            s.weekly.is_none(),
            "model-scoped windows are not the weekly budget"
        );
    }

    #[test]
    fn merge_fills_missing_windows() {
        let now = Utc::now();
        let old = Snapshot {
            provider: ProviderId::Claude,
            session: Some(Window {
                used_pct: 5.0,
                resets_at: None,
            }),
            weekly: Some(Window {
                used_pct: 40.0,
                resets_at: None,
            }),
            plan: Some("pro".into()),
            fetched: now,
            origin: Origin::Endpoint,
        };
        let new = Snapshot {
            session: Some(Window {
                used_pct: 9.0,
                resets_at: None,
            }),
            weekly: None,
            plan: None,
            origin: Origin::Harness,
            ..old.clone()
        }
        .merged_over(&old);
        assert_eq!(new.session.unwrap().used_pct, 9.0);
        assert_eq!(new.weekly.unwrap().used_pct, 40.0);
    }

    #[test]
    fn times_parse_both_ways() {
        let iso = serde_json::json!("2099-04-11T07:00:00.528743+00:00");
        assert!(parse_time(Some(&iso)).is_some());
        let secs = serde_json::json!(1_900_000_000);
        let ms = serde_json::json!(1_900_000_000_000_u64);
        assert_eq!(parse_time(Some(&secs)), parse_time(Some(&ms)));
    }
}
