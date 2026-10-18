//! Claude Pro/Max usage from `api.anthropic.com/api/oauth/usage`.
//!
//! The token is Claude Code's own OAuth login. subr never refreshes it:
//! Anthropic rotates refresh tokens on use, so refreshing here would sign
//! Claude Code out. When the token has expired or is refused, the
//! credentials are read again (Claude Code refreshes them as it runs) and
//! the request is tried once more.

use std::path::PathBuf;

use chrono::Utc;
use serde_json::Value;
use tokio::sync::Mutex;

use super::{
    Origin, Pacer, ProviderId, QuotaError, QuotaProvider, Snapshot, Window, clamp_pct, first,
    parse_time,
};

const URL: &str = "https://api.anthropic.com/api/oauth/usage";

#[derive(Debug)]
pub struct Claude {
    url: String,
    client: reqwest::Client,
    pacer: Mutex<Pacer>,
}

#[derive(Debug, Clone)]
struct Credentials {
    token: String,
    plan: Option<String>,
    expires_ms: Option<i64>,
}

impl Claude {
    pub fn new(url: Option<String>, min_fetch_secs: u64) -> Self {
        Self {
            url: url.unwrap_or_else(|| URL.to_owned()),
            client: super::http(),
            pacer: Mutex::new(Pacer::new(min_fetch_secs)),
        }
    }

    async fn fetch(
        &self,
        creds: &Credentials,
    ) -> Result<Value, (QuotaError, Option<reqwest::Response>)> {
        let resp = self
            .client
            .get(&self.url)
            .bearer_auth(&creds.token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| (QuotaError::Http(e.without_url().to_string()), None))?;
        match resp.status().as_u16() {
            200..=299 => resp
                .json::<Value>()
                .await
                .map_err(|e| (QuotaError::Parse(e.to_string()), None)),
            401 | 403 => Err((QuotaError::Unauthorized, None)),
            429 => Err((QuotaError::RateLimited(Default::default()), Some(resp))),
            s => Err((QuotaError::Http(format!("HTTP {s}")), None)),
        }
    }
}

#[async_trait::async_trait]
impl QuotaProvider for Claude {
    async fn usage(&self) -> Result<Snapshot, QuotaError> {
        let mut pacer = self.pacer.lock().await;
        if let Some(cached) = pacer.cached() {
            return cached;
        }
        let creds = read_credentials()?;
        if creds
            .expires_ms
            .is_some_and(|e| e - 60_000 < Utc::now().timestamp_millis())
        {
            // Expired: Claude Code refreshes it the next time it runs.
            tracing::debug!("claude token expired; using it anyway in case it was refreshed");
        }
        let mut result = self.fetch(&creds).await;
        if matches!(result, Err((QuotaError::Unauthorized, _))) {
            let again = read_credentials()?;
            if again.token != creds.token {
                result = self.fetch(&again).await;
            }
        }
        match result {
            Ok(raw) => {
                let snap = parse(&raw, creds.plan.clone())?;
                pacer.ok(&snap);
                Ok(snap)
            }
            Err((QuotaError::RateLimited(_), resp)) => {
                let wait = pacer.rate_limited(resp.as_ref().and_then(super::retry_after));
                Err(QuotaError::RateLimited(wait))
            }
            Err((e, _)) => Err(e),
        }
    }
}

fn credentials_path() -> PathBuf {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".claude")
        });
    dir.join(".credentials.json")
}

fn read_credentials() -> Result<Credentials, QuotaError> {
    if let Some(c) = std::fs::read_to_string(credentials_path())
        .ok()
        .and_then(|t| parse_credentials(&t))
    {
        return Ok(c);
    }
    #[cfg(target_os = "macos")]
    if let Some(c) = keychain() {
        return Ok(c);
    }
    if let Ok(token) = std::env::var("CLAUDE_CODE_OAUTH_TOKEN")
        && !token.trim().is_empty()
    {
        return Ok(Credentials {
            token: token.trim().to_owned(),
            plan: None,
            expires_ms: None,
        });
    }
    Err(QuotaError::NoCredentials(
        "no Claude Code login found (run `claude` and sign in)".to_owned(),
    ))
}

#[cfg(target_os = "macos")]
fn keychain() -> Option<Credentials> {
    let out = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
        .ok()?;
    out.status.success().then_some(())?;
    parse_credentials(&String::from_utf8_lossy(&out.stdout))
}

fn parse_credentials(text: &str) -> Option<Credentials> {
    let v: Value = serde_json::from_str(text).ok()?;
    let o = v.get("claudeAiOauth")?;
    let token = o.get("accessToken")?.as_str()?.trim();
    if token.is_empty() {
        return None;
    }
    Some(Credentials {
        token: token.to_owned(),
        plan: first(o, &["subscriptionType", "rateLimitTier"])
            .and_then(Value::as_str)
            .map(str::to_owned),
        expires_ms: o.get("expiresAt").and_then(Value::as_i64),
    })
}

/// Reads both response shapes: the older `five_hour` / `seven_day` objects
/// and the newer `limits` list.
pub(super) fn parse(raw: &Value, plan: Option<String>) -> Result<Snapshot, QuotaError> {
    let mut session = None;
    let mut weekly = None;
    if let Some(limits) = raw.get("limits").and_then(Value::as_array) {
        for l in limits {
            let kind = first(l, &["kind", "type", "id"])
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            let Some(w) = window(
                l,
                &["percent", "utilization", "used_percent", "usedPercent"],
            ) else {
                continue;
            };
            match kind.as_str() {
                "session" | "five_hour" | "five-hour" | "5h" => session = Some(w),
                "weekly_all" | "seven_day" | "seven-day" | "7d" | "week" => weekly = Some(w),
                _ => {}
            }
        }
    }
    if session.is_none() {
        session = raw
            .get("five_hour")
            .and_then(|b| window(b, &["utilization"]));
    }
    if weekly.is_none() {
        weekly = raw
            .get("seven_day")
            .and_then(|b| window(b, &["utilization"]));
    }
    if session.is_none() && weekly.is_none() {
        return Err(QuotaError::Parse(
            "no five_hour or seven_day window".to_owned(),
        ));
    }
    Ok(Snapshot {
        provider: ProviderId::Claude,
        session,
        weekly,
        plan,
        fetched: Utc::now(),
        origin: Origin::Endpoint,
    })
}

fn window(obj: &Value, keys: &[&str]) -> Option<Window> {
    let used = first(obj, keys)?.as_f64()?;
    Some(Window {
        used_pct: clamp_pct(used),
        resets_at: parse_time(first(obj, &["resets_at", "resetsAt", "reset_at"])),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::QuotaProvider;
    use serde_json::json;
    use wiremock::matchers::{header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn parses_window_objects() {
        let raw = json!({
            "five_hour": {"utilization": 33.0, "resets_at": "2099-04-11T07:00:00.528743+00:00"},
            "seven_day": {"utilization": 12.5, "resets_at": null},
            "seven_day_opus": {"utilization": 99.0},
            "extra_usage": {"is_enabled": false}
        });
        let s = parse(&raw, Some("max".into())).unwrap();
        assert_eq!(s.session.as_ref().unwrap().used_pct, 33.0);
        assert!(s.session.unwrap().resets_at.is_some());
        assert_eq!(s.weekly.unwrap().used_pct, 12.5);
    }

    #[test]
    fn parses_limits_list() {
        let raw = json!({"limits": [
            {"kind": "session", "percent": 4, "resets_at": "2099-01-01T00:00:00Z"},
            {"kind": "weekly_all", "percent": 61},
            {"kind": "weekly_scoped", "percent": 100, "display_name": "Opus"}
        ]});
        let s = parse(&raw, None).unwrap();
        assert_eq!(s.session.unwrap().used_pct, 4.0);
        assert_eq!(s.weekly.unwrap().used_pct, 61.0);
    }

    #[test]
    fn clamps_and_rejects_empty() {
        let s = parse(&json!({"seven_day": {"utilization": 140}}), None).unwrap();
        assert_eq!(s.weekly.unwrap().used_pct, 100.0);
        assert!(parse(&json!({"error": "x"}), None).is_err());
    }

    #[test]
    fn reads_credentials_file() {
        let c = parse_credentials(
            r#"{"claudeAiOauth":{"accessToken":"t","refreshToken":"r","expiresAt":1,"subscriptionType":"max"}}"#,
        )
        .unwrap();
        assert_eq!(c.token, "t");
        assert_eq!(c.plan.as_deref(), Some("max"));
        assert!(parse_credentials(r#"{"claudeAiOauth":{"accessToken":""}}"#).is_none());
    }

    // One test touches the environment so others can't race it.
    #[tokio::test]
    async fn fetches_caches_and_backs_off() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"tok"}}"#,
        )
        .unwrap();
        // SAFETY of the test: no other test reads CLAUDE_CONFIG_DIR.
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", dir.path()) };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(header("authorization", "Bearer tok"))
            .and(header("anthropic-beta", "oauth-2025-04-20"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "five_hour": {"utilization": 10.0}, "seven_day": {"utilization": 20.0}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let c = Claude::new(Some(server.uri()), 60);
        assert_eq!(c.usage().await.unwrap().weekly.unwrap().used_pct, 20.0);
        // Served from cache: the mock expects exactly one request.
        assert_eq!(c.usage().await.unwrap().weekly.unwrap().used_pct, 20.0);

        let limited = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "120"))
            .expect(1)
            .mount(&limited)
            .await;
        let c = Claude::new(Some(limited.uri()), 60);
        match c.usage().await {
            Err(QuotaError::RateLimited(d)) => assert_eq!(d.as_secs(), 120),
            other => panic!("expected rate limit, got {other:?}"),
        }
        // Still blocked, without another request.
        assert!(matches!(c.usage().await, Err(QuotaError::RateLimited(_))));
    }
}
