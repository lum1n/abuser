//! End to end: the real `subr` binary runs a queued task through acc with a
//! fake Claude Code, against a mock usage endpoint and a bare git remote.
//! Everything lives in a temporary `$HOME`.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Speaks just enough of Claude Code's stream-json: on the first prompt it
/// reports a rate-limit window, commits a file and ends the turn with a
/// `subr-result` block.
const FAKE_CLAUDE: &str = r##"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"type":"user"'*)
      printf '%s\n' '{"type":"system","subtype":"init","session_id":"fake-1","cwd":"/","model":"fake","tools":[],"permissionMode":"acceptEdits"}'
      printf '%s\n' '{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"seven_day","utilization":0.21,"resetsAt":RESETS},"session_id":"fake-1"}'
      echo hello > hello.txt
      git add hello.txt && git -c user.name=t -c user.email=t@t commit -qm "Add hello.txt"
      printf '%s\n' '{"type":"assistant","message":{"id":"msg_1","type":"message","role":"assistant","model":"fake","content":[{"type":"text","text":"Added it.\n\n```subr-result\n{\"status\":\"done\",\"summary\":\"Added hello.txt\",\"pr_title\":\"Add hello.txt\"}\n```"}]},"session_id":"fake-1"}'
      printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"Added it.","session_id":"fake-1","num_turns":1,"total_cost_usd":0,"usage":{"input_tokens":10,"output_tokens":5}}'
      ;;
  esac
done
"##;

fn sh(dir: &Path, script: &str) -> String {
    let out = Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{script}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

struct Env {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
}

impl Env {
    fn subr(&self, args: &[&str]) -> std::process::Output {
        let home = self.root.join("home");
        Command::new(env!("CARGO_BIN_EXE_subr"))
            .args(args)
            .env("HOME", &home)
            .env("SUBR_CONFIG", self.root.join("config.toml"))
            .env("SUBR_STATE_DIR", self.root.join("state"))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.subr(args);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "subr {args:?} failed:\n{text}");
        text
    }
}

const RESETS: i64 = 1_900_000_000;

/// A temporary `$HOME` with a Claude login, a repo with a bare remote, the
/// fake harness and a config pointing at `usage_url`.
fn setup(fake_claude: &str, usage_url: &str, extra_config: &str) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("home");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(
        home.join(".claude/.credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"test-token","subscriptionType":"max"}}"#,
    )
    .unwrap();

    std::fs::create_dir_all(root.join("repos")).unwrap();
    sh(
        &root,
        &format!(
            "git init -q --bare remote.git \
             && git init -q -b main repos/proj && cd repos/proj \
             && echo '# proj' > README.md && git add README.md \
             && git -c user.name=t -c user.email=t@t commit -qm init \
             && git remote add origin {}/remote.git && git push -q origin main \
             && git remote set-head origin main",
            root.display()
        ),
    );

    let fake = root.join("bin/claude");
    std::fs::create_dir_all(fake.parent().unwrap()).unwrap();
    std::fs::write(&fake, fake_claude.replace("RESETS", &RESETS.to_string())).unwrap();
    sh(&root, &format!("chmod +x {}", fake.display()));

    std::fs::write(
        root.join("config.toml"),
        format!(
            r#"
[repos]
roots = ["{repos}"]

[jobs]
watch_secs = 10

[providers.claude]
usage_url = "{usage_url}"
min_fetch_secs = 1
launcher = {{ program = "{fake}" }}
{extra_config}

[providers.codex]
enabled = false
"#,
            repos = root.join("repos").display(),
            fake = fake.display(),
        ),
    )
    .unwrap();
    Env { _tmp: tmp, root }
}

fn usage(weekly: f64) -> serde_json::Value {
    let resets = chrono::DateTime::from_timestamp(RESETS, 0)
        .unwrap()
        .to_rfc3339();
    serde_json::json!({
        "five_hour": {"utilization": 5.0, "resets_at": null},
        "seven_day": {"utilization": weekly, "resets_at": resets},
    })
}

/// Runs `subr <args>` off the test's runtime, which serves the mock endpoint.
async fn subr_async(env: &std::sync::Arc<Env>, args: &'static [&'static str]) -> String {
    let env = env.clone();
    tokio::time::timeout(
        Duration::from_secs(240),
        tokio::task::spawn_blocking(move || env.ok(args)),
    )
    .await
    .expect("subr took too long")
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn queued_task_becomes_a_pushed_branch() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(usage(20.0)))
        .mount(&server)
        .await;
    let env = std::sync::Arc::new(setup(FAKE_CLAUDE, &server.uri(), ""));
    let root = env.root.clone();
    let home = root.join("home");

    let status = env.ok(&["status"]);
    assert!(status.contains("week 20%"), "{status}");
    assert!(status.contains("may start a job"), "{status}");

    let repo = root.join("repos/proj");
    env.ok(&["add", "Add a hello file", "--repo", repo.to_str().unwrap()]);
    assert!(env.ok(&["tasks"]).contains("Add a hello file"));

    let once = subr_async(&env, &["once"]).await;
    assert!(once.contains("done"), "{once}");

    // The work arrived on the remote as a subr/ branch, with the file.
    let branches = sh(&root.join("remote.git"), "git branch --list 'subr/*'");
    let branch = branches.trim().trim_start_matches("* ").to_owned();
    assert!(branch.starts_with("subr/"), "remote branches: {branches:?}");
    let files = sh(
        &root.join("remote.git"),
        &format!("git ls-tree --name-only {branch}"),
    );
    assert!(files.contains("hello.txt"), "{files}");

    // The harness reported 21% against the endpoint's 20%: 1 point charged.
    let jobs = env.ok(&["jobs"]);
    assert!(jobs.contains("done") && jobs.contains("1.0%"), "{jobs}");

    // The worktree and acc session are cleaned up; the task is closed, and
    // with the queue empty the repo's presets are next.
    assert_eq!(
        std::fs::read_dir(home.join(".acc/w"))
            .map(|d| d.count())
            .unwrap_or(0),
        0
    );
    assert_eq!(
        std::fs::read_dir(home.join(".acc/s"))
            .map(|d| d.count())
            .unwrap_or(0),
        0
    );
    let tasks = env.ok(&["tasks"]);
    assert!(
        !tasks.contains("Add a hello file") && tasks.contains("review-recent in proj"),
        "{tasks}"
    );

    // What the job did stays readable after it is gone.
    let job = env.ok(&["job", "hello"]);
    assert!(
        job.contains("Add a hello file") && job.contains("Added it."),
        "{job}"
    );
    assert!(!job.contains("subr-result"), "{job}");
    let logs = env.ok(&["logs"]);
    assert!(
        logs.contains("agent  Added it.") && logs.contains("turn  done"),
        "{logs}"
    );

    // A preset can be skipped, then asked for again.
    env.ok(&["skip", "review-recent"]);
    assert!(!env.ok(&["tasks"]).contains("review-recent in proj"));
    let next = env.ok(&["run-now", "review-recent", "--repo", "proj"]);
    assert!(next.contains("daemon isn't running"), "{next}");
    let tasks = env.ok(&["tasks"]);
    assert!(
        tasks.contains("  1. review-recent in proj") && tasks.contains("(next)"),
        "{tasks}"
    );
    assert!(env.ok(&["status"]).contains("next: preset:"));

    // A queued task can be skipped, retried and removed.
    env.ok(&["add", "Second thing", "--repo", repo.to_str().unwrap()]);
    env.ok(&["skip", "second"]);
    assert!(
        env.ok(&["tasks"])
            .contains("Second thing: skipped with `subr skip`")
    );
    env.ok(&["retry", "second"]);
    assert!(env.ok(&["tasks"]).contains(". Second thing  [task"));
    let queue = env.ok(&["queue"]);
    assert!(queue.contains("Second thing"), "{queue}");
    env.ok(&["queue", "rm", "2"]);
    assert!(env.ok(&["queue"]).contains("the queue is empty"));

    // Settings change in place.
    env.ok(&["config", "set", "jobs.max_attempts", "3"]);
    let cfg = std::fs::read_to_string(root.join("config.toml")).unwrap();
    assert!(
        cfg.contains("max_attempts = 3") && cfg.contains("watch_secs = 10"),
        "{cfg}"
    );
    assert!(
        !env.subr(&["config", "set", "jobs.max_attempts", "lots"])
            .status
            .success()
    );
}

/// Never finishes: reports in, then works forever and ignores interrupts.
const ENDLESS_CLAUDE: &str = r##"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"type":"user"'*)
      printf '%s\n' '{"type":"system","subtype":"init","session_id":"fake-2","cwd":"/","model":"fake","tools":[],"permissionMode":"acceptEdits"}'
      echo partial > partial.txt
      ;;
  esac
done
"##;

/// Usage that rises 2 points with every request after the first two.
struct Ramp(std::sync::atomic::AtomicU32);

impl wiremock::Respond for Ramp {
    fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
        let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ResponseTemplate::new(200).set_body_json(usage(20.0 + 2.0 * f64::from(n.saturating_sub(2))))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn job_is_stopped_at_the_daily_limit() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(Ramp(std::sync::atomic::AtomicU32::new(0)))
        .mount(&server)
        .await;
    let env = std::sync::Arc::new(setup(ENDLESS_CLAUDE, &server.uri(), "daily_limit_pct = 5"));
    let repo = env.root.join("repos/proj");
    env.ok(&["add", "Work forever", "--repo", repo.to_str().unwrap()]);

    let once = subr_async(&env, &["once"]).await;
    assert!(once.contains("budget_stopped"), "{once}");
    assert!(once.contains("daily limit reached"), "{once}");

    // Stopped at 4 (limit 5 minus the 1-point margin), not later: with
    // readings 2 points apart, the most it can show is the reading that
    // crossed 4, plus the one taken after the agent was stopped.
    let jobs = env.ok(&["jobs"]);
    let spent: f64 = jobs
        .split_whitespace()
        .find_map(|w| w.strip_suffix('%')?.parse().ok())
        .unwrap();
    assert!((4.0..=6.0).contains(&spent), "spent {spent}: {jobs}");

    // The partial work stays for a look; nothing was pushed; no more jobs today.
    let kept: Vec<_> = std::fs::read_dir(env.root.join("home/.acc/w"))
        .unwrap()
        .flatten()
        .collect();
    assert!(kept.iter().any(|e| e.path().join("partial.txt").exists()));
    assert!(
        sh(&env.root.join("remote.git"), "git branch --list 'subr/*'")
            .trim()
            .is_empty()
    );
    let diff = env.ok(&["job", "--diff"]);
    assert!(diff.contains("?? partial.txt"), "{diff}");

    let again = env.ok(&["once", "--dry-run"]);
    assert!(
        again.contains("not starting") && again.contains("today"),
        "{again}"
    );
}
