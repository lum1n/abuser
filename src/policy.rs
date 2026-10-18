//! The approvals an unattended agent gets without asking: reading, and
//! building and testing in its worktree. Everything else is rejected.
//!
//! Builds and tests run the project's own code, so for repos you don't
//! trust, start the harness in a sandbox (`launcher.prefix`).

use acc_core::policy::{Decision, Rule};

/// Command prefixes allowed by default (whole words; acc never lets an
/// allow rule match a chained command).
const ALLOW: &[&str] = &[
    // Looking around.
    "ls",
    "cat",
    "head",
    "tail",
    "wc",
    "grep",
    "rg",
    "find",
    "tree",
    "pwd",
    "which",
    "diff",
    "file",
    "git status",
    "git diff",
    "git log",
    "git show",
    "git blame",
    "git grep",
    "git ls-files",
    // Rust, Go, JavaScript, Python, Swift, JVM, generic.
    "cargo build",
    "cargo check",
    "cargo test",
    "cargo clippy",
    "cargo fmt",
    "cargo nextest",
    "go build",
    "go test",
    "go vet",
    "gofmt",
    "golangci-lint",
    "npm test",
    "npm run",
    "pnpm test",
    "pnpm run",
    "pnpm exec",
    "pnpm lint",
    "yarn test",
    "yarn run",
    "bun test",
    "bun run",
    "npx tsc",
    "npx eslint",
    "npx prettier",
    "npx vitest",
    "npx jest",
    "pytest",
    "python -m pytest",
    "python3 -m pytest",
    "python -m unittest",
    "python3 -m unittest",
    "ruff",
    "mypy",
    "swift build",
    "swift test",
    "mvn test",
    "./gradlew test",
    "make",
    "just",
];

/// Never, whatever else is configured: publishing is subr's job.
const DENY: &[&str] = &["git push", "gh", "git remote", "git config"];

fn shell(decision: Decision, command: &str) -> Rule {
    Rule {
        decision,
        tool: Some("shell".to_owned()),
        path: None,
        command: Some(command.to_owned()),
    }
}

/// The rules a job runs with: the built-in ones (unless turned off), then
/// the configured ones. Deny rules win over allow rules in acc.
pub fn rules(configured: &[Rule], defaults: bool) -> Vec<Rule> {
    let mut out: Vec<Rule> = DENY.iter().map(|c| shell(Decision::Deny, c)).collect();
    if defaults {
        out.push(Rule {
            decision: Decision::Allow,
            tool: Some("read".to_owned()),
            path: None,
            command: None,
        });
        out.extend(ALLOW.iter().map(|c| shell(Decision::Allow, c)));
    }
    out.extend(configured.iter().cloned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use acc_core::policy::{Policy, Request, ToolKind};

    fn decide(rules: Vec<Rule>, command: &str) -> Option<Decision> {
        let req = Request {
            tool: Some("Bash".to_owned()),
            kind: Some(ToolKind::Shell),
            command: Some(command.to_owned()),
            paths: Vec::new(),
        };
        Policy { rules }.decide(&req, "/w").map(|(d, _)| d)
    }

    #[test]
    fn tests_yes_publishing_no() {
        let r = rules(&[], true);
        assert_eq!(decide(r.clone(), "cargo test --all"), Some(Decision::Allow));
        assert_eq!(
            decide(r.clone(), "python -m unittest test_calc -v"),
            Some(Decision::Allow)
        );
        assert_eq!(
            decide(r.clone(), "git push origin main"),
            Some(Decision::Deny)
        );
        assert_eq!(
            decide(r.clone(), "cargo test && git push"),
            Some(Decision::Deny)
        );
        assert_eq!(decide(r.clone(), "curl https://example.com"), None);
        // Without defaults only the denials and the configured rules apply.
        let extra = vec![shell(Decision::Allow, "curl")];
        assert_eq!(decide(rules(&extra, false), "cargo test"), None);
        assert_eq!(
            decide(rules(&extra, false), "curl x"),
            Some(Decision::Allow)
        );
    }
}
