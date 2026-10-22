# ADR 0001: The software factory is a new repo on an engine pulled out of abuser

- Status: proposed
- Date: 2026-10-22
- Deciders: Vegard Svendsen

"factory" and "engine" are working names. The real names are still open (see [Open questions](#open-questions)).

This ADR lives in abuser because the factory repo doesn't exist yet and the first piece of work (M1) happens here. When the factory repo is created, this file is copied there as its ADR 0001, and abuser keeps it as the record of why its code was split.

## Context

We want to sell a self-hosted software factory: an issue goes in, and a planned, implemented, verified and reviewed pull request comes out. A human approves every merge. Buyers are small to mid-size dev teams and agencies, Nordic first. It ships as a single binary or a Docker image, with managed runners as an optional extra.

abuser already does the middle of that pipeline for one person:
- task sources (`src/tasks/`: native, markdown, github, lapp, command)
- running an agent through acc in its own worktree (`src/harness.rs`, `src/runner.rs`)
- the result contract (`src/prompt.rs`, `parse_result`)
- an approval policy for unattended agents (`src/policy.rs`)
- committing, verifying, pushing and opening a draft PR (`src/publish.rs`, `src/gh.rs`)
- a SQLite store for jobs, attempts, cooldowns and the native queue (`src/store.rs`)

But abuser is built around something the product must not do: spending a personal subscription's spare quota.
- What may start, and when a job must stop, is decided by quota readings from private endpoints (`src/quota/`, `src/budget.rs`, the `Meter` and `must_stop` in `src/runner.rs`).
- The launcher strips API keys so the agent always runs on the subscription login (`subscription_launcher`, `api_key_vars`). Using a personal subscription this way is fine for a personal tool, and not something a product can be sold on.
- It has one stage (implement), one user, no server, no auth and no sandbox.
- Names are hard-coded: the `abuser-result` fence, `abuser/…` branches, the PR footer with quota percentages, the `ABUSER_*` variables.

## Options

### A. Evolve abuser into the factory

Add stages, a server, auth and a sandbox to this repo, behind config.

- Reuses everything with no extraction work.
- Every factory feature would have to keep working with quota budgeting, and every abuser fix would have to keep working in a multi-user server. The two would pull the same code in different directions.
- abuser's purpose (spend a personal subscription) would ship in the product binary, which is the wrong thing to put in front of buyers and their lawyers.
- abuser's README, config and users would carry server, tenant and licence-key concepts they will never use.
- The paid parts need a different licence. Keeping them out of an MIT repo is easier than fencing them inside one.

### B. New repo, copy abuser's code in

- Fast to start, and each side is free to change.
- The two copies drift at once. Fixes to sources, publishing or the acc driver have to be made twice, and abuser is the one that gets daily use, so it is where those bugs are found.

### C. New repo, on a shared engine crate pulled out of abuser (chosen)

Move the generic parts of abuser into an `engine` crate in its own repo. abuser and the factory both depend on it by git revision, the same way abuser depends on acc today.

- Bugs found by running abuser every day get fixed once, for both.
- Each product keeps only what is its own: abuser its quota budget and subscription launcher, the factory its stages, server, sandbox and cost limits.
- The licence line follows crate lines (see [Open and paid](#open-and-paid)).
- Costs: one to two weeks of extraction (M1), and changes to engine that need a commit there, then a pin bump in each user.

## Decision

**Option C.** The factory is a new repo. abuser stays a personal quota spender. Both depend on an MIT `engine` crate in its own repo, pulled out of abuser in M1. engine gets its own repo rather than a place in the acc workspace: acc is about driving harnesses, and engine is about jobs, task sources and publishing on top of it.

### What moves to engine

| From abuser | Into engine |
|---|---|
| `src/tasks/` (all sources) | as is |
| `src/publish.rs`, `src/gh.rs` | as is, with prefixes and the PR footer passed in |
| `src/harness.rs` (acc driver) | as is |
| `src/prompt.rs` | the result contract, with the fence name passed in, and results per stage |
| `src/policy.rs` | the default rules |
| `src/store.rs` | the `jobs`, `attempts`, `cooldowns`, `native_tasks` and `kv` tables, `JobStatus` |
| config types | `PublishCfg`, `JobsCfg`, `ReposCfg`, the `SourceCfg` variants |
| `paths::state_dir` | takes the product's name instead of reading `ABUSER_*` |

Two new seams replace what is abuser-specific today:
- **`Governor`** with `can_start`, `on_usage` and `must_stop`. abuser implements it as `QuotaGovernor` over `src/quota/` and `src/budget.rs`. The factory implements `CostGovernor`: USD per job, USD per tenant per day, jobs per runner, and wall-clock time.
- **A launcher provider** replacing `ProviderId` and `subscription_launcher`. abuser's strips API keys. The factory's builds sandbox → `keep run` → harness, using the customer's own keys.

Staying in abuser: `src/quota/`, `src/budget.rs`, the `quota` and `charges` tables, `daemon.rs`, `select.rs`, the TUI, the service installer and the presets.

**M1 is done when** abuser's `tests/e2e.rs` passes unchanged against engine, nothing changes for abuser's users, and the governor has unit tests of its own.

## The factory pipeline

### Stages

Each job goes through these stages in order. Brackets mark a stage that is off unless a repo turns it on.

| Stage | Who acts | Output |
|---|---|---|
| `queued` | – | The job: source, task key, repo, title, body, labels, requester |
| `planning` | agent, read only | `{status, plan_md, files_expected[], risks[], questions[]}` |
| [`plan_review`] | human | approved or edited plan |
| `implementing` | agent | `{status: done\|blocked\|no_change, summary, pr_title, pr_body}`, changes left uncommitted |
| `verifying` | factory, in the sandbox | `{passed, steps[{cmd, exit, tail}]}` |
| `publishing` | factory | commit, push `factory/<job>`, open a draft PR (or push to it on later loops) |
| `reviewing` | a different harness or model, read only | `{verdict, findings[{file, line, severity, title, body, suggestion?}]}`, posted as a PR review |
| `ci` | GitHub | the required checks, from `gh pr checks --watch` |
| `awaiting_approval` | human | the PR is marked ready and reviewers are asked; approved or rejected |
| `merging` | factory | `gh pr merge` (squash by default), then the worktree is removed |

The PR is opened as a draft before review so CI runs while the reviewer reads. Agent stages end with a fenced `factory-result` JSON block, and the last block that parses is the result, as in abuser.

### Job state machine

A job has one `state`: one of the stages above, or one of these.

| State | Kind | Meaning |
|---|---|---|
| `needs_human` | parked | Waiting for a person: open questions from planning, a loop limit, a governor stop, a sandbox denial, or a diff that needs a `sandbox_escalation` approval. Records the stage it parked from. |
| `merged` | final | The PR is merged. |
| `no_change` | final | The agent found nothing to do. |
| `blocked` | final | The agent said it can't be done. |
| `rejected` | final | A human rejected the plan or the PR. The PR is closed. |
| `failed` | final | The factory hit an error it couldn't recover from. |
| `cancelled` | final | A human cancelled it. |

Transitions:

```
queued        → planning
planning      → plan_review | implementing      (plan_review if the repo turns it on)
              → needs_human                     (open questions)
plan_review   → implementing | rejected
implementing  → verifying | no_change | blocked
verifying     → publishing                      (passed)
              → implementing                    (failed, verify_loops < max_verify_loops)
              → needs_human                     (failed, limit reached)
publishing    → reviewing
reviewing     → ci                              (no blocker or major findings)
              → implementing                    (blocker or major, review_loops < max_review_loops)
              → needs_human                     (limit reached)
ci            → awaiting_approval               (green)
              → implementing                    (red, ci_loops < max_ci_loops)
              → needs_human                     (limit reached)
awaiting_approval → merging | rejected | implementing   (approve, reject, or ask for changes)
merging       → merged | needs_human            (merge refused, for example by branch protection)
needs_human   → <the stage it parked from> | cancelled | rejected
any non-final → cancelled | failed
```

Rules:
- **Every transition is an event** in an append-only `events` table (`seq, tenant, job_id, at, type, payload_json`). The job's state, the UI, the metrics and the audit log are all built from it. A transition caused by a person names them.
- **Loop counters live on the job**: `verify_loops`, `review_loops`, `ci_loops`, `human_change_loops`. Each loop back to `implementing` adds one to the counter of the stage that sent it back. Resuming from `needs_human` after a loop limit allows one more loop and is recorded.
- **Merging is never automatic.** The only ways into `merging` are an approval in the factory by someone allowed to approve, or a person merging on GitHub (seen by webhook, recorded as an approval by them). With the four-eyes rule on, the approver can't be the requester.
- **One worktree per job, one acc session per stage.** Implement and fix loops may carry context over with acc's `Command::Handoff`. At most one open job per issue and per branch.
- **The governor is asked before each agent stage** and watches it while it runs. A stop parks the job in `needs_human` with the worktree kept.
- **Final states stay final.** Retrying a `blocked`, `failed` or `rejected` job makes a new job linked to the old one, so a job's history never runs backwards.

## Open and paid

The line follows crates, so nobody has to read a crate to know its licence.

**Open, MIT.** Everything one person needs to run the whole pipeline on their own machine:
- acc, observer and engine
- `factory-core`: jobs, stages, the state machine, the event log, the scheduler
- `factory-sandbox`: the bwrap and seatbelt profiles
- `factory-cli`: `factory serve` for one user, the local runner, `factory job …`
- intake from GitHub issues, the native queue and Lapp

**Paid, source-available.** What a team needs, and what a single user doesn't:
- in `factory-server`: more than one user, tenants, roles, OIDC and SSO, the audit log, approval rules (four-eyes), remote and managed runners
- `factory-review`: the cross-model review stage
- intake from Linear, Jira and GitLab
- the web UI's team screens (the approval inbox, members, metrics)

A signed ed25519 licence key (tenant, seats, expiry, features) turns the paid features on. It is checked offline, so it works air-gapped.

Why this line:
- The free tier is useful on its own, so people can try the pipeline before talking to us.
- Teams pay for what only teams need. The review stage is paid because it is the main thing that makes the PRs worth merging, and it doubles model cost, so it fits the people who buy.
- Nothing about safety is paid: the sandbox, the deny policy and the human merge gate are open.
- The product never runs on a personal subscription's quota. Customers use their own API keys or business seats, and abuser's key-stripping launcher stays in abuser.

## Consequences

- abuser loses most of its code to engine and gains a `Governor` and a launcher provider. Its behaviour, config and tests stay the same.
- M1 waits for abuser's tree to be clean, and is done on a branch in abuser with the e2e tests as the gate.
- engine starts at a git-pinned revision, like acc. It is not published to crates.io while its API moves.
- Changes to engine need a commit there and a pin bump in each user. That is the cost of fixing things once.
- The hard-coded names (result fence, branch, job and label prefixes, PR footer, state dir and variables) become parameters, so engine has no mention of abuser.

## Open questions

These don't change the decision above, and are settled before M1 starts:
1. The product's name, and engine's.
2. The paid crates' licence: BUSL 1.1 (converting to MIT after some years), an Elastic-style licence, or proprietary. BUSL is the current preference: buyers can read and audit the code they self-host.
3. Price per seat or per runner.
