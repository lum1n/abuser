//! ChatGPT/Codex usage from `chatgpt.com/backend-api/wham/usage`, with the
//! login Codex keeps in `$CODEX_HOME/auth.json`. API-key logins have no
//! subscription quota, so they count as not signed in.

use std::path::PathBuf;

use chrono::Utc;
use serde_json::Value;
use tokio::sync::Mutex;

use super::{
    Origin, Pacer, ProviderId, QuotaError, QuotaProvider, Snapshot, Window, clamp_pct, first,
    parse_time,
};

const URL: &str = "https://chatgpt.com/backend-api/wham/usage";

#[derive(Debug)]
pub struct Codex {
    url: String,
    client: reqwest::Client,
    pacer: Mutex<Pacer>,
}

#[derive(Debug, Clone, PartialEq)]
struct Auth {
    token: String,
    account_id: Option<String>,
}

impl Codex {
    pub fn new(url: Option<String>, min_fetch_secs: u64) -> Self {
        Self {
            url: url.unwrap_or_else(|| URL.to_owned()),
            client: super::http(),
            pacer: Mutex::new(Pacer::new(min_fetch_secs)),
        }
    }
}

#[async_trait::async_trait]
impl QuotaProvider for Codex {
    async fn usage(&self) -> Result<Snapshot, QuotaError> {
        let mut pacer = self.pacer.lock().await;
        if let Some(cached) = pacer.cached() {
            return cached;
        }
        let path = auth_path();
        let auth = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| parse_auth(&t))
            .ok_or_else(|| {
                QuotaError::NoCredentials(
                    "no ChatGPT login for Codex (run `codex login`)".to_owned(),
                )
            })?;
        let mut req = self
            .client
            .get(&self.url)
            .bearer_auth(&auth.token)
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(id) = &auth.account_id {
            req = req.header("ChatGPT-Account-Id", id);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| QuotaError::Http(e.without_url().to_string()))?;
        match resp.status().as_u16() {
            200..=299 => {
                let raw: Value = resp
                    .json()
                    .await
                    .map_err(|e| QuotaError::Parse(e.to_string()))?;
                let snap = parse(&raw)?;
                pacer.ok(&snap);
                Ok(snap)
            }
            401 | 403 => Err(QuotaError::Unauthorized),
            429 => Err(QuotaError::RateLimited(
                pacer.rate_limited(super::retry_after(&resp)),
            )),
            s => Err(QuotaError::Http(format!("HTTP {s}"))),
        }
    }
}

fn auth_path() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".codex")
        })
        .join("auth.json")
}

fn parse_auth(text: &str) -> Option<Auth> {
    let v: Value = serde_json::from_str(text).ok()?;
    let mode = first(&v, &["auth_mode", "authMode"])
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(mode.as_str(), "apikey" | "api_key" | "api-key") {
        return None;
    }
    let tokens = v.get("tokens").filter(|t| t.is_object())?;
    let token = first(tokens, &["access_token", "accessToken"])?
        .as_str()?
        .trim();
    if token.is_empty() {
        return None;
    }
    Some(Auth {
        token: token.to_owned(),
        account_id: first(tokens, &["account_id", "accountId"])
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// Windows are told apart by length where the response gives it: some
/// plans have only a primary window, and it is the weekly one.
pub(super) fn parse(raw: &Value) -> Result<Snapshot, QuotaError> {
    let rate = first(raw, &["rate_limit", "rateLimit"]).unwrap_or(raw);
    let mut session = None;
    let mut weekly = None;
    for (keys, positional_weekly) in [
        (&["primary_window", "primary"][..], false),
        (&["secondary_window", "secondary"][..], true),
    ] {
        let Some(b) = first(rate, keys) else { continue };
        let Some(w) = window(b) else { continue };
        let is_weekly = match window_minutes(b) {
            Some(m) => m >= 24 * 60,
            None => positional_weekly,
        };
        if is_weekly {
            weekly.get_or_insert(w);
        } else {
            session.get_or_insert(w);
        }
    }
    if session.is_none() && weekly.is_none() {
        return Err(QuotaError::Parse(
            "no primary or secondary window".to_owned(),
        ));
    }
    Ok(Snapshot {
        provider: ProviderId::Codex,
        session,
        weekly,
        plan: first(raw, &["plan_type", "planType"])
            .and_then(Value::as_str)
            .map(str::to_owned),
        fetched: Utc::now(),
        origin: Origin::Endpoint,
    })
}

fn window_minutes(b: &Value) -> Option<u64> {
    if let Some(m) =
        first(b, &["window_duration_mins", "windowDurationMins"]).and_then(Value::as_u64)
    {
        return Some(m);
    }
    first(b, &["limit_window_seconds", "limitWindowSeconds"])
        .and_then(Value::as_u64)
        .map(|s| s / 60)
}

fn window(b: &Value) -> Option<Window> {
    let used = first(b, &["used_percent", "usedPercent"])?.as_f64()?;
    Some(Window {
        used_pct: clamp_pct(used),
        resets_at: parse_time(first(b, &["reset_at", "resets_at", "resetsAt", "resetAt"])),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_wham_usage() {
        let raw = json!({
            "plan_type": "plus",
            "rate_limit": {
                "primary_window": {"used_percent": 7, "limit_window_seconds": 18000, "reset_at": 1900000000},
                "secondary_window": {"used_percent": 44.5, "window_duration_mins": 10080, "reset_at": 1900500000}
            }
        });
        let s = parse(&raw).unwrap();
        assert_eq!(s.session.unwrap().used_pct, 7.0);
        let w = s.weekly.unwrap();
        assert_eq!(w.used_pct, 44.5);
        assert_eq!(w.resets_at.unwrap().timestamp(), 1_900_500_000);
        assert_eq!(s.plan.as_deref(), Some("plus"));
    }

    #[test]
    fn a_lone_weekly_primary_window_is_weekly() {
        let raw = json!({"plan_type": "go", "rate_limit": {
            "primary_window": {"used_percent": 3, "limit_window_seconds": 604800, "reset_at": 1900000000},
            "secondary_window": null
        }});
        let s = parse(&raw).unwrap();
        assert!(s.session.is_none());
        assert_eq!(s.weekly.unwrap().used_pct, 3.0);
    }

    #[test]
    fn auth_skips_api_keys() {
        assert!(parse_auth(r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk"}"#).is_none());
        assert!(parse_auth(r#"{"OPENAI_API_KEY":"sk"}"#).is_none());
        let a = parse_auth(r#"{"tokens":{"access_token":"t","account_id":"acct"}}"#).unwrap();
        assert_eq!(a.account_id.as_deref(), Some("acct"));
    }
}
