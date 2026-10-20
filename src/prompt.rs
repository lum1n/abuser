//! The prompt an agent gets, and the result block it must end with.

use serde::{Deserialize, Serialize};

use crate::presets::Mode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultStatus {
    Done,
    Blocked,
    NoChange,
}

/// What the agent reports in its final ```` ```abuser-result ```` block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentResult {
    pub status: ResultStatus,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub pr_title: Option<String>,
    #[serde(default)]
    pub pr_body: Option<String>,
}

pub struct PromptInput<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub mode: Mode,
    pub branch: &'a str,
    pub hint: Option<String>,
    pub verify: Option<&'a str>,
}

pub fn build(p: &PromptInput<'_>) -> String {
    let mut out = String::new();
    out.push_str(
        "You are working unattended: nobody will answer questions or approve anything \
         while you work. Make sensible decisions yourself and note them in your summary.\n\n\
         First check that the task can be done here, by you alone. If it needs things you \
         don't have (other hardware or operating systems, accounts, credentials, devices, \
         a human's review or decision), stop at once and report `blocked` with what is \
         missing. Don't do partial work around it.\n\n",
    );
    out.push_str("## Task\n\n");
    out.push_str(p.title.trim());
    out.push('\n');
    if !p.body.trim().is_empty() {
        out.push('\n');
        out.push_str(p.body.trim());
        out.push('\n');
    }
    if let Some(h) = &p.hint {
        out.push('\n');
        out.push_str(h);
        out.push('\n');
    }
    out.push_str("\n## Rules\n\n");
    match p.mode {
        Mode::Pr => {
            out.push_str(&format!(
                "- You are in a git worktree of its own (branch `{}`). Leave your changes uncommitted: \
                 they are committed, pushed and opened as a pull request for you.\n\
                 - Don't run git commands that change anything (add, commit, push, branch, checkout).\n\
                 - Keep the change focused on the task. Follow the project's conventions (read its README, \
                 CONTRIBUTING, AGENTS.md or CLAUDE.md if present).\n\
                 - Run the project's tests and linters for what you touched, and fix what you broke.\n",
                p.branch
            ));
            if let Some(v) = p.verify {
                out.push_str(&format!("- Before you finish, `{v}` must pass.\n"));
            }
        }
        Mode::Report => out.push_str(
            "- This is read only: don't change, commit or delete files.\n\
             - Your summary is the deliverable; make it complete and specific.\n",
        ),
    }
    out.push_str(
        "- If the task can't be done, or needs a human decision, stop and say why.\n\n\
         ## Finish\n\n\
         End your final message with exactly one fenced block like this:\n\n\
         ```abuser-result\n\
         {\"status\": \"done\", \"summary\": \"What you did and why, in Markdown.\", \
         \"pr_title\": \"Short imperative title\", \"pr_body\": \"Markdown description for reviewers.\"}\n\
         ```\n\n\
         `status` is `done` (finished), `no_change` (nothing needed doing) or `blocked` (couldn't \
         finish; the summary says why). `pr_title` and `pr_body` are only needed with `done`.\n",
    );
    out
}

const FENCE: &str = "```abuser-result";

/// The last `abuser-result` block in `text` that parses. The JSON is read as
/// one value, so fences inside its strings (code in `pr_body`) don't end it.
pub fn parse_result(text: &str) -> Option<AgentResult> {
    text.match_indices(FENCE)
        .filter_map(|(i, _)| {
            let after = &text[i + FENCE.len()..];
            serde_json::Deserializer::from_str(after)
                .into_iter::<AgentResult>()
                .next()?
                .ok()
        })
        .last()
}

/// The message without its result block, for summaries.
pub fn strip_result(text: &str) -> String {
    match text.rfind(FENCE) {
        Some(i) => text[..i].trim_end().to_owned(),
        None => text.trim().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_last_result_block() {
        let text = "Did things.\n\n```abuser-result\n{\"status\":\"blocked\",\"summary\":\"first\"}\n```\nThen more.\n```abuser-result\n{\"status\": \"done\", \"summary\": \"Fixed it\", \"pr_title\": \"Fix X\"}\n```";
        let r = parse_result(text).unwrap();
        assert_eq!(r.status, ResultStatus::Done);
        assert_eq!(r.pr_title.as_deref(), Some("Fix X"));
        assert_eq!(strip_result(text).lines().last(), Some("Then more."));
    }

    #[test]
    fn fences_inside_the_json() {
        let text = "Done.\n```abuser-result\n{\"status\": \"done\", \"pr_title\": \"Fix\", \"pr_body\": \"Tests:\\n```\\nok\\n```\\n\"}\n```";
        let r = parse_result(text).unwrap();
        assert_eq!(r.pr_title.as_deref(), Some("Fix"));
        assert!(r.pr_body.unwrap().contains("```\nok"));
    }

    #[test]
    fn missing_or_broken_block() {
        assert!(parse_result("no block here").is_none());
        assert!(parse_result("```abuser-result\n{not json}\n```").is_none());
        assert!(parse_result("```abuser-result\n{\"status\":\"done\"").is_none());
    }

    #[test]
    fn prompt_mentions_branch_and_verify() {
        let p = build(&PromptInput {
            title: "Add retries",
            body: "",
            mode: Mode::Pr,
            branch: "acc/abuser-x",
            hint: None,
            verify: Some("cargo test"),
        });
        assert!(p.contains("`acc/abuser-x`"));
        assert!(p.contains("`cargo test` must pass"));
        assert!(p.contains("```abuser-result"));
    }
}
