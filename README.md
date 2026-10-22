# abuser

abuser runs in the background and spends what's left of your AI coding subscriptions (Claude Pro/Max, ChatGPT/Codex) on useful work. When quota is free, it picks the next task from your todo lists, issue tracker or its own queue. If nothing is queued, it runs standing chores (reviews, tests, lint fixes, doc updates) in the repositories you have worked on most recently. Each result arrives as a draft pull request.

The name is short for *abonnement user*: "abonnement" is Norwegian for subscription.

It never goes over a daily limit you set. By default that limit is a seventh of the weekly window per day. Your own use counts too: abuser fills only what you leave, so the week lasts until it resets.

```
$ abuser status
claude (max)
  5h 0%, resets Mon 00:20  ·  week 65%, resets Tue 03:00
  today: 3.0% of 14.3% used  ·  may start a job
codex (plus)
  5h ?  ·  week 4%, resets Tue 18:20
  today: 0.0% of 14.3% used  ·  may start a job
```

## How it works

1. **Read quota.** abuser reads each subscription's 5-hour and weekly windows from the provider's usage endpoint, using the login the CLI already keeps. While a job runs, it also reads the rate-limit reports the harness sends.
2. **Decide.** A job may start when all of these hold:
   - the reading is fresh;
   - the weekly window is below `max_weekly_pct`;
   - the 5-hour window is below `max_session_pct`;
   - at least `min_job_pct` of today's budget is left;
   - the time is inside `schedule.windows`, if you set any.
3. **Pick work.** abuser takes queued tasks first, starting with the repo you committed to most recently. When the queue is empty, it takes presets that are due, round-robin over the repos you've been active in.
4. **Run it.** The agent runs in its own git worktree. It is started through [acc](https://github.com/lum1n/acc), which drives Claude Code and Codex headlessly. abuser removes API keys (`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `OPENAI_API_KEY`, `CODEX_API_KEY`) from the harness's environment, so it always runs on the subscription login and never bills an API account. Your checkout is never touched.
5. **Publish.**
   1. abuser commits the work.
   2. It runs your `verify` command, if you set one.
   3. It pushes a `abuser/…` branch and opens a **draft PR** (GitHub, via `gh`).
   4. It tells the task source: a Lapp todo is ticked, a GitHub issue gets `Closes #N`, and so on.

   A repo with `publish.max_open_prs` (default 2) open `abuser` PRs gets no new PR work until you merge or close some. `abuser tasks` marks that work as waiting.

### The daily limit

Spend is measured, not estimated. While a job runs, every rise in the weekly window counts against abuser's daily budget, including anything you use yourself at the same time. This over-counts on purpose, so abuser can't under-measure its way past the limit, and it backs off while you're busy.

The weekly window also sets a pace: by the end of a budget day, it may be `daily_limit_pct` full for each 24 hours of the window up to then, counting your own use. The window rarely starts at `day_starts`, so the budget days it starts and ends in get a share of the limit for the hours they cover; whatever `day_starts` is, the days add up to the week. Today's budget is used up by abuser's own spend or by the window reaching that pace, whichever comes first. If you used 6% on the first day of the week, abuser has 8.3% of 14.3% left. Budget left over from quiet days carries forward to the pace, but abuser still never spends more than `daily_limit_pct` itself in a day.

abuser checks the budget at least every `watch_secs` while a job runs. The job is stopped when one of these happens:
- today's budget is used up, less `stop_margin_pct`;
- the weekly window passes `max_weekly_pct`;
- no fresh reading has arrived for `stale_kill_after_secs`.

When a job is stopped, the agent is interrupted, then killed. Its worktree is kept so you can look at it.

Readings are about one percent apart, so `stop_margin_pct` (default 1) absorbs the last interval.

abuser fails closed: with no reading, nothing starts. When the daemon stops, it stops the running agent too, so nothing spends while it isn't watching.

## Install

You need Rust 1.88 or later, git, and the CLIs you want abuser to use, signed in:
- `claude` (Claude Code)
- `codex`
- `gh` (for pull requests and issues)

```sh
cargo install --git https://github.com/lum1n/abuser
abuser config example > ~/.config/abuser/config.toml   # then set repos.roots
abuser config check
abuser once --dry-run          # what would run now, and why
abuser install-service         # systemd user unit, or launchd agent on macOS; (re)starts it
```

## Commands

| Command | What it does |
|---|---|
| `abuser run` | Run in the foreground; this is what the service runs. |
| `abuser tui` | Watch and steer abuser in one screen: quota, jobs with their live logs and changes, and the plan. Pause, stop, add, skip, retry and run next from there (`?` lists the keys). |
| `abuser status [--json]` | Whether the daemon runs, quota windows, today's spend, and the running and recent jobs. |
| `abuser tasks [--json]` | The work abuser would pick, in order, and what it skips and why. |
| `abuser once [--dry-run]` | One round now. |
| `abuser add "title" --repo <path or remote>` | Queue a task in abuser's own queue. |
| `abuser queue` / `abuser queue rm <id>` | List the queue, or remove a task from it. |
| `abuser skip <task>` | Leave a task out until `abuser retry`, or put a preset off for a cooldown. |
| `abuser retry <task>` | Try a task again that was skipped, gave up or is waiting to retry. |
| `abuser run-now <task>` / `abuser run-now <preset> --repo <repo>` | Run it next, whatever its attempts, cooldown or `publish.max_open_prs` say. The budget still decides when. |
| `abuser jobs` | Recent jobs with what they spent and their PRs. |
| `abuser job [<job>] [--diff]` | One job: its summary, spend and files, or its changes. |
| `abuser logs [<job>] [-f]` | What the agent did, step by step; `-f` follows a running job. |
| `abuser pause` / `abuser resume` | Stop or restart starting jobs. |
| `abuser stop <job>` | Stop a running job; its worktree is discarded. |
| `abuser config set <key> <value>` / `unset <key>` / `edit` | Change the config, keeping its comments. The daemon reloads it. |
| `abuser config check \| example \| path` | Check the setup, print the example config, or show the file locations. |

A `<task>` is its number or part of its title as `abuser tasks` shows it. A `<job>` is all or part of its id; without one, the running job or else the latest.

## Configuration

`~/.config/abuser/config.toml`, or `$ABUSER_CONFIG`. Every setting is optional except `repos.roots`. See [`config.example.toml`](config.example.toml) for the full list. The main ones:

```toml
[repos]
roots = ["~/src"]

[providers.claude]
daily_limit_pct = 14.285714   # of the weekly window, per day
max_weekly_pct = 85
model = "claude-sonnet-5"

[providers.codex]
enabled = false

[[sources]]
type = "github"               # issues labelled `abuser`

[repo."github.com/you/project"]
verify = "cargo test"
```

### Task sources

| `type` | Where tasks come from | What happens when a job is done |
|---|---|---|
| `native` | `abuser add` | The task is closed. |
| `markdown` | `- [ ]` items in `TODO.md` (set with `file`) | The agent ticks the box on the branch. |
| `github` | Open issues with a label (`label`, default `abuser`) | The PR closes the issue. If abuser gives up, it comments on the issue. |
| `lapp` | Todos in [Lapp](https://github.com/lum1n/lapp) notes, through its MCP server (`command`) | `complete_todo` and `append_log` are called. |
| `command` | Any program: `<cmd> list` prints `[{"id","title","body","repo"}]` | `<cmd> complete <id>` or `<cmd> fail <id>` gets the outcome as JSON on stdin. |

A task is tried at most `jobs.max_attempts` times. It is never tried again once it has a PR, or once the agent reports it `blocked` or `no_change`. `[tasks] exclude` skips titles containing any of the given words, which is useful for todos only a human can do.

### Presets

Presets run when nothing is queued, once per repo per cooldown period:

| Preset | What it does | Result |
|---|---|---|
| `review-recent` | Fixes clear bugs in the last two weeks of commits | PR |
| `tests` | Adds tests for recent changes | PR |
| `lint-fix` | Fixes warnings from the linters the repo already uses | PR |
| `docs-sync` | Brings the docs up to date with the code | PR |
| `deps-audit` | Reports outdated or vulnerable dependencies | Report |

A report goes to Lapp as a note when the Lapp source is set up, and otherwise to `~/.local/state/abuser/reports/`.

You can turn presets off, change their prompt, mode or `cooldown_days`, or add your own under `[presets.<name>]`.

### What the agent may do

The agent runs unattended, so every approval it asks for is answered by policy:
- **Questions** get the answer "decide yourself and note it".
- **File edits** in the worktree are allowed through the harness's own mode (`acceptEdits` for Claude, `:workspace` for Codex).
- **Built-in rules** allow reading and the usual build, test and lint commands.
- **Always denied:** `git push`, `gh`, `git remote` and `git config`.
- **Everything else** is rejected.

Add your own rules with `policy = [{ decision = "allow", tool = "shell", command = "docker compose" }]`. Builds and tests run the project's code, so for repos you don't trust, run the harness in a sandbox with `launcher = { prefix = ["bwrap", ...] }`.

## Privacy and credentials

abuser reads the OAuth logins Claude Code (`~/.claude/.credentials.json`, or the Keychain on macOS) and Codex (`~/.codex/auth.json`) already keep. It only uses them to read usage, and it never refreshes, stores or logs them.

The usage endpoints are private and undocumented. They may change without notice, and abuser then stops starting jobs until it is updated.

State lives in `~/.local/state/abuser/` (or `$ABUSER_STATE_DIR`). Each job's transcript and token usage are kept in `jobs/<id>/`.

## Development

```sh
cargo test            # unit tests, plus end-to-end runs against a fake harness
cargo clippy --all-targets
```

The end-to-end tests run the real binary in a temporary `$HOME`, with a fake Claude Code, a mock usage endpoint and a bare git remote. One checks that a queued task becomes a pushed branch. The other checks that a job is stopped at the daily limit while usage climbs.

## License

MIT. See [LICENSE](LICENSE).
