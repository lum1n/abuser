//! Drawing the TUI from its state.

use chrono::{Local, Utc};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};

use super::state::{App, Focus, JobText, Mode, PlanItem};
use crate::budget::Verdict;
use crate::daemon::Standing;
use crate::quota::Window;
use crate::store::{JobRow, JobStatus};

const DIM: Style = Style::new().fg(Color::DarkGray);

pub fn draw(f: &mut Frame, app: &App) {
    let providers = app.data.standings.as_ref().map_or(1, |s| s.len().max(1)) as u16;
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(providers + 2),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .areas(f.area());
    draw_header(f, app, header);

    let [left, right] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(body);
    let [jobs, plan] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(left);
    draw_jobs(f, app, jobs);
    draw_plan(f, app, plan);
    draw_detail(f, app, right);
    draw_footer(f, app, footer);

    match &app.mode {
        Mode::Normal => {}
        Mode::Help => draw_help(f),
        Mode::Confirm { prompt, .. } => {
            let area = popup(f.area(), (prompt.len() as u16 + 4).min(f.area().width), 3);
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(prompt.as_str()).block(Block::bordered().title(" confirm ")),
                area,
            );
        }
        Mode::Add(form) => {
            let area = popup(f.area(), 70.min(f.area().width), 4);
            f.render_widget(Clear, area);
            let field = |i: usize, label: &str| {
                let style = if form.at == i {
                    Style::new().bold()
                } else {
                    Style::new()
                };
                let cursor = if form.at == i { "▏" } else { "" };
                Line::from(vec![
                    Span::styled(format!("{label:>6}  "), DIM),
                    Span::styled(format!("{}{cursor}", form.fields[i]), style),
                ])
            };
            f.render_widget(
                Paragraph::new(vec![field(0, "title"), field(1, "repo")]).block(
                    Block::bordered()
                        .title(" add a task ")
                        .title_bottom(" enter: next / add · tab: switch · esc: cancel "),
                ),
                area,
            );
        }
    }
}

fn popup(area: Rect, width: u16, height: u16) -> Rect {
    let [v] = Layout::vertical([Constraint::Length(height)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    let [h] = Layout::horizontal([Constraint::Length(width)])
        .flex(ratatui::layout::Flex::Center)
        .areas(v);
    h
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let d = &app.data;
    let mut top = vec![
        Span::styled(" abuser ", Style::new().bold().reversed()),
        " ".into(),
    ];
    match &d.heartbeat {
        Some(h) if h.alive(Utc::now()) => {
            top.push(Span::styled("● ", Style::new().green()));
            top.push(format!("daemon: {}", h.doing).into());
        }
        Some(h) => {
            top.push(Span::styled("● ", Style::new().red()));
            top.push(
                format!(
                    "daemon not running (last seen {})",
                    h.at.with_timezone(&Local).format("%a %H:%M")
                )
                .into(),
            );
        }
        None => {
            top.push(Span::styled("● ", Style::new().red()));
            top.push("daemon not seen".into());
        }
    }
    if d.paused {
        top.push("  ".into());
        top.push(Span::styled(" paused ", Style::new().black().on_yellow()));
    }
    if let Some(next) = app.next_title() {
        top.push(Span::styled(format!("  next: {next}"), Style::new().cyan()));
    }
    let mut lines = vec![Line::from(top)];
    match &d.standings {
        None => lines.push(Line::styled(" reading quota…", DIM)),
        Some(s) => lines.extend(s.iter().map(standing_line)),
    }
    f.render_widget(
        Paragraph::new(lines).block(Block::new().borders(ratatui::widgets::Borders::BOTTOM)),
        area,
    );
}

fn bar(used: f64, of: f64, width: usize) -> Span<'static> {
    let frac = if of > 0.0 {
        (used / of).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let full = (frac * width as f64).round() as usize;
    let color = if frac < 0.6 {
        Color::Green
    } else if frac < 0.85 {
        Color::Yellow
    } else {
        Color::Red
    };
    Span::styled(
        format!("{}{}", "█".repeat(full), "░".repeat(width - full)),
        Style::new().fg(color),
    )
}

fn window(label: &str, w: Option<&Window>) -> Vec<Span<'static>> {
    let mut out = vec![Span::styled(format!("{label} "), DIM)];
    match w {
        Some(w) => {
            out.push(bar(w.used_pct, 100.0, 10));
            out.push(format!(" {:>3.0}%", w.used_pct).into());
            if let Some(r) = w.resets_at {
                out.push(Span::styled(
                    format!(" ↻{}", r.with_timezone(&Local).format("%a %H:%M")),
                    DIM,
                ));
            }
        }
        None => out.push(Span::styled("?", DIM)),
    }
    out
}

fn standing_line(s: &Standing) -> Line<'static> {
    let snap = s.snapshot.as_ref();
    let plan = snap.and_then(|s| s.plan.clone()).unwrap_or_default();
    let mut spans = vec![Span::styled(
        format!(" {:<7}{:<6}", s.provider.as_str(), plan),
        Style::new().bold(),
    )];
    spans.extend(window("5h", snap.and_then(|s| s.session.as_ref())));
    spans.push("   ".into());
    spans.extend(window("week", snap.and_then(|s| s.weekly.as_ref())));
    spans.push(Span::styled("   today ", DIM));
    spans.push(bar(s.used_today, s.daily_limit, 8));
    spans.push(format!(" {:.1} of {:.1}%   ", s.used_today, s.daily_limit).into());
    spans.push(match &s.verdict {
        Verdict::Go => Span::styled("may start a job", Style::new().green()),
        Verdict::No(why) => Span::styled(format!("waiting: {why}"), Style::new().yellow()),
    });
    if let Some(e) = &s.error {
        spans.push(Span::styled(format!("  ({e})"), Style::new().red()));
    }
    Line::from(spans)
}

fn pane(title: String, focused: bool) -> Block<'static> {
    let b = Block::bordered().title(title);
    if focused {
        b.border_type(BorderType::Thick)
            .border_style(Style::new().cyan())
    } else {
        b.border_style(DIM)
    }
}

pub fn status_style(s: JobStatus) -> Style {
    match s {
        JobStatus::Running => Style::new().yellow().bold(),
        JobStatus::Done => Style::new().green(),
        JobStatus::NoChange | JobStatus::Skipped => DIM,
        JobStatus::Blocked | JobStatus::BudgetStopped | JobStatus::TimedOut => {
            Style::new().magenta()
        }
        JobStatus::Failed => Style::new().red(),
    }
}

fn draw_jobs(f: &mut Frame, app: &App, area: Rect) {
    let rows = app.data.jobs.iter().map(|j| {
        Row::new(vec![
            Cell::from(
                j.started
                    .with_timezone(&Local)
                    .format("%m-%d %H:%M")
                    .to_string(),
            )
            .style(DIM),
            Cell::from(j.status.as_str()).style(status_style(j.status)),
            Cell::from(format!("{:>4.1}%", j.spent)),
            Cell::from(j.title.clone()),
        ])
    });
    let running = app
        .data
        .jobs
        .iter()
        .filter(|j| j.status == JobStatus::Running)
        .count();
    let title = if running > 0 {
        format!(" jobs · {running} running ")
    } else {
        " jobs ".to_owned()
    };
    let table = Table::new(
        rows,
        [
            Constraint::Length(11),
            Constraint::Length(14),
            Constraint::Length(5),
            Constraint::Fill(1),
        ],
    )
    .block(pane(title, app.focus == Focus::Jobs))
    .row_highlight_style(highlight(app.focus == Focus::Jobs));
    let mut state = TableState::default().with_selected(Some(app.job_sel));
    f.render_stateful_widget(table, area, &mut state);
}

fn highlight(focused: bool) -> Style {
    if focused {
        Style::new().reversed()
    } else {
        Style::new().add_modifier(Modifier::BOLD)
    }
}

fn draw_plan(f: &mut Frame, app: &App, area: Rect) {
    let block = pane(
        match app.data.plan_at {
            Some(t) => format!(" plan · as of {} ", t.with_timezone(&Local).format("%H:%M")),
            None => " plan ".to_owned(),
        },
        app.focus == Focus::Plan,
    );
    let Some(items) = &app.data.plan else {
        f.render_widget(
            Paragraph::new("reading tasks…").style(DIM).block(block),
            area,
        );
        return;
    };
    let mut n = 0;
    let rows = items.iter().map(|p| {
        let is_next = p.key.is_some() && p.key == app.data.next;
        let num = if p.work.is_some() {
            n += 1;
            format!("{n:>2}.")
        } else {
            "  –".to_owned()
        };
        let style = if p.work.is_none() {
            DIM
        } else if is_next {
            Style::new().cyan()
        } else if p.why.is_some() {
            Style::new().yellow()
        } else {
            Style::new()
        };
        Row::new(vec![
            Cell::from(num),
            Cell::from(if is_next {
                "next".to_owned()
            } else if p.work.is_some() && p.why.is_some() {
                "waiting".to_owned()
            } else {
                p.kind.clone()
            }),
            Cell::from(p.title.clone()),
        ])
        .style(style)
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Length(16),
            Constraint::Fill(1),
        ],
    )
    .block(block)
    .row_highlight_style(highlight(app.focus == Focus::Plan));
    let mut state = TableState::default().with_selected(Some(app.plan_sel));
    f.render_stateful_widget(table, area, &mut state);
}

fn field(label: &str, value: impl Into<String>) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<9}"), DIM),
        Span::raw(value.into()),
    ])
}

fn job_text(app: &App, j: &JobRow) -> Text<'static> {
    let mut lines = vec![Line::styled(j.title.clone(), Style::new().bold())];
    lines.push(field("id", j.id.clone()));
    lines.push(field("repo", j.repo.clone()));
    let ran = (j.ended.unwrap_or_else(Utc::now) - j.started).num_minutes();
    lines.push(Line::from(vec![
        Span::styled(format!("{:<9}", "status"), DIM),
        Span::styled(j.status.as_str().to_owned(), status_style(j.status)),
        format!(
            "  ·  {}  ·  {:.1}% of the weekly window  ·  {}m",
            j.provider, j.spent, ran
        )
        .into(),
    ]));
    if let Some(pr) = &j.pr_url {
        lines.push(field("pr", pr.clone()));
    }
    if let Some(d) = j.detail.as_deref().filter(|d| !d.is_empty()) {
        lines.push(field("detail", d.to_owned()));
    }
    let text = app.texts.get(&j.id);
    if app.show_diff {
        lines.push(Line::styled("── changes ──", DIM));
        match text.and_then(|t| t.diff.as_ref()) {
            None => lines.push(Line::styled("reading…", DIM)),
            Some(Err(e)) => lines.push(Line::styled(e.clone(), Style::new().red())),
            Some(Ok(d)) if d.trim().is_empty() => lines.push(Line::styled("no changes", DIM)),
            Some(Ok(d)) => lines.extend(d.lines().map(|l| {
                let style = if l.starts_with('+') {
                    Style::new().green()
                } else if l.starts_with('-') {
                    Style::new().red()
                } else if l.starts_with("@@") {
                    Style::new().cyan()
                } else if l.starts_with("diff ") || l.starts_with("??") {
                    Style::new().bold()
                } else {
                    Style::new()
                };
                Line::styled(l.to_owned(), style)
            })),
        }
        return Text::from(lines);
    }
    match text {
        Some(t) if !t.log.is_empty() => {
            lines.push(Line::styled("── log ──", DIM));
            for l in &t.log {
                let style = match l.kind {
                    "agent" => Style::new().white(),
                    "run" => Style::new().cyan(),
                    "edit" => Style::new().green(),
                    "error" => Style::new().red(),
                    "ask" => Style::new().yellow(),
                    _ => DIM,
                };
                let mut rest = l.text.lines();
                lines.push(Line::from(vec![
                    Span::styled(format!("{:>8} ", l.kind), style),
                    Span::raw(rest.next().unwrap_or("").to_owned()),
                ]));
                lines.extend(rest.map(|r| Line::raw(format!("{:>8} {r}", ""))));
            }
        }
        Some(JobText {
            summary: Some(s), ..
        }) => {
            lines.push(Line::styled("── summary ──", DIM));
            lines.extend(s.lines().map(|l| Line::raw(l.to_owned())));
        }
        _ => lines.push(Line::styled("no log", DIM)),
    }
    Text::from(lines)
}

fn plan_text(app: &App, p: &PlanItem) -> Text<'static> {
    let mut lines = vec![Line::styled(p.title.clone(), Style::new().bold())];
    lines.push(field("kind", p.kind.clone()));
    if !p.repo.is_empty() {
        lines.push(field("repo", p.repo.clone()));
    }
    if let Some(k) = &p.key {
        lines.push(field("key", k.clone()));
    }
    if let Some(why) = &p.why {
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    "{:<9}",
                    if p.work.is_some() {
                        "waiting"
                    } else {
                        "skipped"
                    }
                ),
                DIM,
            ),
            Span::styled(why.clone(), Style::new().yellow()),
        ]));
    }
    if p.key.is_some() && p.key == app.data.next {
        lines.push(Line::styled("runs next", Style::new().cyan()));
    }
    if !p.body.trim().is_empty() {
        lines.push(Line::raw(""));
        lines.extend(p.body.lines().map(|l| Line::raw(l.to_owned())));
    }
    Text::from(lines)
}

fn draw_detail(f: &mut Frame, app: &App, area: Rect) {
    let text = match app.focus {
        Focus::Jobs => app.selected_job().map(|j| job_text(app, j)),
        Focus::Plan => app.selected_plan().map(|p| plan_text(app, p)),
    };
    let follow = if app.scroll_back == 0 {
        " following "
    } else {
        " scrolled · f follows "
    };
    let block = pane(" detail ".to_owned(), false).title_bottom(Line::styled(follow, DIM));
    let Some(text) = text else {
        f.render_widget(
            Paragraph::new("nothing selected").style(DIM).block(block),
            area,
        );
        return;
    };
    let para = Paragraph::new(text).wrap(Wrap { trim: false });
    let inner = block.inner(area);
    let total = para.line_count(inner.width) as u16;
    let top = total
        .saturating_sub(inner.height)
        .saturating_sub(app.scroll_back);
    f.render_widget(para.block(block).scroll((top, 0)), area);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let line = match &app.message {
        Some((m, true)) => Line::styled(format!(" {m}"), Style::new().red()),
        Some((m, false)) => Line::styled(format!(" {m}"), Style::new().green()),
        None => {
            let keys: &[(&str, &str)] = match app.focus {
                Focus::Jobs => &[
                    ("tab", "plan"),
                    ("s", "stop"),
                    ("d", "diff"),
                    ("o", "open PR"),
                ],
                Focus::Plan => &[
                    ("tab", "jobs"),
                    ("r", "run next"),
                    ("x", "skip"),
                    ("R", "retry"),
                    ("D", "remove"),
                ],
            };
            let common = [
                ("a", "add"),
                ("p", if app.data.paused { "resume" } else { "pause" }),
                ("e", "config"),
                ("?", "help"),
                ("q", "quit"),
            ];
            let mut spans = vec![Span::raw(" ")];
            for (k, what) in keys.iter().chain(common.iter()) {
                spans.push(Span::styled(*k, Style::new().bold().cyan()));
                spans.push(Span::styled(format!(" {what}  "), DIM));
            }
            Line::from(spans)
        }
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_help(f: &mut Frame) {
    let keys = super::state::KEYS;
    let area = popup(f.area(), 52.min(f.area().width), keys.len() as u16 + 2);
    f.render_widget(Clear, area);
    let lines: Vec<Line> = keys
        .iter()
        .map(|(k, what)| {
            Line::from(vec![
                Span::styled(format!(" {k:<17}"), Style::new().bold().cyan()),
                Span::raw(*what),
            ])
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" keys · any key closes ")),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::state::tests::{job, native};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(app: &App) -> String {
        let mut t = Terminal::new(TestBackend::new(140, 30)).unwrap();
        t.draw(|f| draw(f, app)).unwrap();
        let buf = t.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn shows_jobs_plan_and_the_selected_log() {
        let mut app = App::default();
        app.set_jobs(vec![job("abuser-1-run", JobStatus::Running)]);
        let item = native("1", "Fix the flaky test");
        app.data.next = item.key.clone();
        app.set_plan(vec![item]);
        app.texts.insert(
            "abuser-1-run".into(),
            JobText {
                log: vec![crate::harness::LogLine {
                    kind: "run",
                    text: "cargo test".into(),
                }],
                ..Default::default()
            },
        );
        let screen = render(&app);
        for want in [
            "daemon not seen",
            "reading quota…",
            "jobs · 1 running",
            "title of abuser-1-run",
            "Fix the flaky test",
            "next: Fix the flaky test",
            "run cargo test",
            "s stop",
        ] {
            assert!(screen.contains(want), "{want:?} missing:\n{screen}");
        }

        app.mode = Mode::Help;
        assert!(render(&app).contains("remove a queued task"));
    }
}
