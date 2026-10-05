//! Drawing: `draw` renders an `App` into a frame. Pure; tested on
//! ratatui's `TestBackend`.

use super::app::{App, Daemon, Tab};
use super::format;
use super::theme::{Palette, Role};
use super::toml_hl;
use crate::ledger::Stats;
use crate::state::SessionInfo;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Sparkline, Table, Tabs, Wrap};

/// Days the Stats tab charts at most (what the snapshot carries).
const CHART_DAYS: usize = crate::ledger::STATS_DAYS as usize;

pub fn draw(f: &mut Frame, app: &App, pal: &Palette) {
    let [head, body, foot] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]).areas(f.area());
    header(f, head, app, pal);
    match app.tab {
        Tab::Overview => overview(f, body, app, pal),
        Tab::Sessions => sessions(f, body, app, pal),
        Tab::Stats => stats(f, body, app, pal),
        Tab::Config => config(f, body, app, pal),
    }
    footer(f, foot, app, pal);
    if app.help {
        help(f, pal);
    }
}

fn header(f: &mut Frame, area: Rect, app: &App, pal: &Palette) {
    let [name, tabs, status] =
        Layout::horizontal([Constraint::Length(17), Constraint::Min(0), Constraint::Length(24)]).areas(area);
    f.render_widget(Span::styled(" claude-presence ", pal.fg(Role::Accent).add_modifier(Modifier::BOLD)), name);
    let titles = Tab::ALL.iter().enumerate().map(|(i, t)| format!("{} {}", i + 1, t.title()));
    let i = Tab::ALL.iter().position(|t| *t == app.tab).unwrap_or(0);
    f.render_widget(
        Tabs::new(titles).select(i).style(pal.fg(Role::Muted)).highlight_style(pal.pill(Role::Accent)).divider(" "),
        tabs,
    );
    let (role, text) = match &app.daemon {
        Daemon::Unknown => (Role::Muted, "connecting…"),
        Daemon::Live if app.paused => (Role::Warn, "paused"),
        Daemon::Live => (Role::Good, "daemon running"),
        Daemon::Busy => (Role::Warn, "daemon busy"),
        Daemon::Old => (Role::Warn, "daemon too old"),
        Daemon::Down(_) => (Role::Bad, "daemon not running"),
    };
    f.render_widget(Line::from(Span::styled(format!(" {text} "), pal.pill(role))).right_aligned(), status);
}

fn footer(f: &mut Frame, area: Rect, app: &App, pal: &Palette) {
    let text = match &app.note {
        Some(n) => Span::styled(format!(" {n}"), pal.fg(Role::Info)),
        None => {
            let r = if app.tab == Tab::Config { "r reload config" } else { "r refresh" };
            Span::styled(format!(" q quit · tab/1-4 switch · j/k scroll · {r} · p pause · ? help"), pal.fg(Role::Muted))
        }
    };
    f.render_widget(text, area);
}

fn block<'a>(title: &'a str, pal: &Palette) -> Block<'a> {
    Block::new()
        .borders(Borders::ALL)
        .border_style(pal.fg(Role::Muted))
        .title(Span::styled(format!(" {title} "), pal.fg(Role::Accent).add_modifier(Modifier::BOLD)))
}

fn kv<'a>(key: &'a str, value: impl Into<Span<'a>>, pal: &Palette) -> Line<'a> {
    Line::from(vec![Span::styled(format!("{key:<10}"), pal.fg(Role::Muted)), value.into()])
}

fn overview(f: &mut Frame, area: Rect, app: &App, pal: &Palette) {
    let [top, card_area, today_area] =
        Layout::vertical([Constraint::Length(6), Constraint::Length(5), Constraint::Min(0)]).areas(area);
    let mut lines = Vec::new();
    match (&app.daemon, &app.snapshot) {
        (Daemon::Down(why), _) => {
            lines.push(kv("daemon", Span::styled("not running", pal.fg(Role::Bad)), pal));
            lines.push(kv("reason", why.as_str(), pal));
            lines.push(Line::styled(
                "Start it with `claude-presence install` or `claude-presence daemon`.",
                pal.fg(Role::Muted),
            ));
        }
        (Daemon::Old, _) => {
            lines.push(kv("daemon", Span::styled("running, but too old for this view", pal.fg(Role::Warn)), pal));
            lines.push(Line::styled("Reinstall to update it: `claude-presence install`.", pal.fg(Role::Muted)));
        }
        (_, Some(s)) => {
            lines.push(kv("daemon", format!("v{} · pid {}", s.version, s.pid), pal));
            let role = match s.discord.as_str() {
                "connected" => Role::Good,
                "bridge" => Role::Info,
                "refused" => Role::Bad,
                _ => Role::Warn,
            };
            let note = match s.discord.as_str() {
                "refused" => " (check client_id)",
                "disconnected" => " (is Discord running?)",
                _ => "",
            };
            lines.push(kv("discord", Span::styled(format!("{}{note}", s.discord), pal.fg(role)), pal));
            lines.push(kv("sessions", s.sessions_total.to_string(), pal));
        }
        _ => lines.push(Line::styled("waiting for the daemon…", pal.fg(Role::Muted))),
    }
    f.render_widget(Paragraph::new(lines).block(block("Daemon", pal)), top);

    let card = app.snapshot.as_ref().and_then(|s| s.card.as_ref());
    let card_lines = match card {
        Some(c) => vec![
            Line::styled(c.details.as_str(), pal.card().add_modifier(Modifier::BOLD)),
            Line::styled(c.state.as_str(), pal.card()),
        ],
        None => vec![Line::styled("nothing shown", pal.fg(Role::Muted))],
    };
    let card_block = block("Discord card", pal).border_style(pal.fg(Role::Discord));
    f.render_widget(Paragraph::new(card_lines).style(pal.card()).block(card_block), card_area);

    let mut today = Vec::new();
    match app.stats() {
        Some(st) => {
            let d = st.days.iter().find(|d| d.day == st.today).cloned().unwrap_or_default();
            today.push(kv("tokens", format::count(d.tokens), pal));
            today.push(kv("prompts", d.prompts.to_string(), pal));
            today.push(kv("active", format::duration(i64::from(d.active_minutes) * 60_000), pal));
            today.push(kv("streak", format!("{} days", st.streak), pal));
        }
        None => today.push(Line::styled(stats_missing(app), pal.fg(Role::Muted))),
    }
    f.render_widget(Paragraph::new(today).block(block("Today", pal)), today_area);
}

fn stats_missing(app: &App) -> String {
    match &app.offline_stats {
        Err(e) if !matches!(app.daemon, Daemon::Live | Daemon::Busy) => format!("stats unavailable: {e}"),
        _ => "no stats yet".into(),
    }
}

fn status_role(status: &str) -> Role {
    match status {
        "thinking" => Role::Thinking,
        "working" => Role::Accent,
        "compacting" => Role::Info,
        "notification" => Role::Warn,
        _ => Role::Muted,
    }
}

fn session_row<'a>(s: &'a SessionInfo, now: i64, pal: &Palette) -> Row<'a> {
    let mark = if s.shown { Span::styled("▶", pal.fg(Role::Discord)) } else { Span::raw(" ") };
    Row::new(vec![
        Cell::from(mark),
        Cell::from(Span::styled(s.status.as_str(), pal.fg(status_role(&s.status)))),
        Cell::from(s.project.as_str()),
        Cell::from(Span::styled(s.branch.as_str(), pal.fg(Role::Muted))),
        Cell::from(s.model.as_str()),
        Cell::from(s.tool.as_str()),
        Cell::from(s.prompts.to_string()),
        Cell::from(format::count(s.tokens)),
        Cell::from(format::duration(now - s.last_event_ms)),
    ])
}

fn sessions(f: &mut Frame, area: Rect, app: &App, pal: &Palette) {
    let Some(snap) = &app.snapshot else {
        let p = Paragraph::new(Line::styled("no live sessions: the daemon is not answering", pal.fg(Role::Muted)));
        return f.render_widget(p.block(block("Sessions", pal)), area);
    };
    let visible = area.height.saturating_sub(3) as usize;
    let skip = (app.scroll as usize).min(snap.sessions.len().saturating_sub(visible));
    let rows = snap.sessions.iter().skip(skip).map(|s| session_row(s, snap.now_ms, pal));
    let widths = [
        Constraint::Length(1),
        Constraint::Length(12),
        Constraint::Fill(2),
        Constraint::Fill(1),
        Constraint::Length(12),
        Constraint::Length(10),
        Constraint::Length(7),
        Constraint::Length(7),
        Constraint::Length(8),
    ];
    let header = Row::new(["", "status", "project", "branch", "model", "tool", "prompts", "tokens", "idle"])
        .style(pal.fg(Role::Muted).add_modifier(Modifier::BOLD));
    let title = if snap.sessions_total as usize > snap.sessions.len() {
        format!("Sessions ({} of {})", snap.sessions.len(), snap.sessions_total)
    } else {
        format!("Sessions ({})", snap.sessions.len())
    };
    f.render_widget(Table::new(rows, widths).header(header).block(block(&title, pal)), area);
}

/// The last `n` days' values ending at `today`, oldest first; days without
/// an entry are zero.
pub fn series(st: &Stats, n: usize, value: impl Fn(&crate::ledger::DayStats) -> u64) -> Vec<u64> {
    let first = st.today - n as i32 + 1;
    let mut out = vec![0; n];
    for d in &st.days {
        if let Some(i) = d.day.checked_sub(first).and_then(|i| usize::try_from(i).ok()).filter(|i| *i < n) {
            out[i] = value(d);
        }
    }
    out
}

fn stats(f: &mut Frame, area: Rect, app: &App, pal: &Palette) {
    let Some(st) = app.stats() else {
        let p = Paragraph::new(Line::styled(stats_missing(app), pal.fg(Role::Muted)));
        return f.render_widget(p.block(block("Stats", pal)), area);
    };
    let [totals, tokens, active] =
        Layout::vertical([Constraint::Length(6), Constraint::Fill(1), Constraint::Fill(1)]).areas(area);
    let u = st.usage;
    let lines = vec![
        kv(
            "tokens",
            format!(
                "{} in · {} out · {} cache read · {} cache write",
                format::count(u.input),
                format::count(u.output),
                format::count(u.cache_read),
                format::count(u.cache_write)
            ),
            pal,
        ),
        kv("prompts", format!("{} · {} turns · {} sessions", st.prompts, st.turns, st.sessions), pal),
        kv("active", format::duration(st.active_ms), pal),
        kv("streak", format!("{} days", st.streak), pal),
    ];
    f.render_widget(Paragraph::new(lines).block(block("Lifetime", pal)), totals);
    let n = (tokens.width.saturating_sub(2) as usize).clamp(1, CHART_DAYS);
    let since = format::short_date(i64::from(st.today) - n as i64 + 1);
    let chart = |title: String, data: Vec<u64>, role: Role, area: Rect, f: &mut Frame| {
        f.render_widget(Sparkline::default().data(&data).style(pal.fg(role)).block(block(&title, pal)), area);
    };
    let tok = series(st, n, |d| d.tokens);
    let peak = format::count(tok.iter().copied().max().unwrap_or(0));
    chart(format!("Tokens per day since {since} (peak {peak})"), tok, Role::Accent, tokens, f);
    let act = series(st, n, |d| u64::from(d.active_minutes));
    chart(format!("Active minutes per day since {since}"), act, Role::Discord, active, f);
}

fn config(f: &mut Frame, area: Rect, app: &App, pal: &Palette) {
    let mut lines: Vec<Line> =
        app.config_notes.iter().map(|n| Line::styled(format!("# ! {n}"), pal.fg(Role::Warn))).collect();
    for l in app.config.as_deref().unwrap_or("").lines() {
        let spans = toml_hl::line(l).into_iter().map(|(role, s)| match role {
            Some(r) => Span::styled(s, pal.fg(r)),
            None => Span::raw(s),
        });
        lines.push(Line::from(spans.collect::<Vec<_>>()));
    }
    let visible = area.height.saturating_sub(2);
    let max = u16::try_from(lines.len()).unwrap_or(u16::MAX).saturating_sub(visible);
    let title = crate::paths::config_file();
    let title = title.display().to_string();
    f.render_widget(Paragraph::new(lines).scroll((app.scroll.min(max), 0)).block(block(&title, pal)), area);
}

fn help(f: &mut Frame, pal: &Palette) {
    let a = f.area();
    let w = a.width.min(52);
    let h = a.height.min(14);
    let area = Rect { x: a.x + (a.width - w) / 2, y: a.y + (a.height - h) / 2, width: w, height: h };
    let keys = [
        ("q, ctrl-c", "quit"),
        ("tab, l / shift-tab, h", "next / previous tab"),
        ("1-4", "go to tab"),
        ("j k, ↑ ↓, pgup pgdn", "scroll"),
        ("g / G", "top / bottom"),
        ("r", "refresh (Config: reload config)"),
        ("p", "pause / resume polling"),
        ("?", "this help"),
    ];
    let lines: Vec<Line> = keys
        .iter()
        .map(|(k, v)| Line::from(vec![Span::styled(format!("{k:<22}"), pal.fg(Role::Accent)), Span::raw(*v)]))
        .collect();
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(block("Keys", pal)).style(Style::new()),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::DayStats;
    use crate::state::{Card, StateSnapshot};
    use crate::tui::app::Update;
    use crate::tui::theme::ColorMode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(app: &App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, app, &Palette { mode: ColorMode::TrueColor })).unwrap();
        let buf = t.backend().buffer();
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    fn live() -> App {
        let mut a = App::default();
        let snap = StateSnapshot {
            v: 1,
            version: "0.2.0".into(),
            pid: 42,
            now_ms: 600_000,
            discord: "connected".into(),
            card: Some(Card { details: "Working on demo".into(), state: "Edit · 1.2k tokens".into() }),
            sessions: vec![SessionInfo {
                id: "abcd1234".into(),
                project: "demo".into(),
                branch: "main".into(),
                status: "working".into(),
                model: "Opus 5.5".into(),
                tool: "Edit".into(),
                last_event_ms: 540_000,
                prompts: 3,
                tokens: 1234,
                shown: true,
                ..SessionInfo::default()
            }],
            sessions_total: 1,
            stats: Some(Stats {
                today: 20_000,
                prompts: 12,
                streak: 4,
                days: vec![DayStats { day: 20_000, tokens: 5000, prompts: 2, active_minutes: 30, turns: 2 }],
                ..Stats::default()
            }),
            ..StateSnapshot::default()
        };
        a.apply(Update::Live(Box::new(snap)));
        a
    }

    #[test]
    fn overview_shows_daemon_card_and_today() {
        let s = render(&live(), 90, 24);
        for want in ["daemon running", "v0.2.0 · pid 42", "connected", "Working on demo", "5.0k", "4 days"] {
            assert!(s.contains(want), "missing {want:?} in\n{s}");
        }
    }

    #[test]
    fn sessions_table() {
        let mut a = live();
        a.tab = Tab::Sessions;
        let s = render(&a, 110, 10);
        for want in ["▶", "working", "demo", "main", "Opus 5.5", "1.2k", "1m"] {
            assert!(s.contains(want), "missing {want:?} in\n{s}");
        }
    }

    #[test]
    fn stats_tab_and_series() {
        let mut a = live();
        a.tab = Tab::Stats;
        let s = render(&a, 80, 24);
        assert!(s.contains("Tokens per day") && s.contains("peak 5.0k"), "{s}");
        let st = a.stats().unwrap();
        assert_eq!(series(st, 3, |d| d.tokens), [0, 0, 5000]);
        let old =
            Stats { today: 10, days: vec![DayStats { day: 2, tokens: 1, ..DayStats::default() }], ..Stats::default() };
        assert_eq!(series(&old, 3, |d| d.tokens), [0, 0, 0]);
    }

    #[test]
    fn daemon_down_falls_back_to_disk() {
        let mut a = App::default();
        a.apply(Update::Down("connection refused".into(), Err("database is locked".into())));
        let s = render(&a, 90, 20);
        assert!(s.contains("daemon not running") && s.contains("connection refused"), "{s}");
        assert!(s.contains("stats unavailable: database is locked"), "{s}");
        a.tab = Tab::Sessions;
        assert!(render(&a, 90, 10).contains("no live sessions"));
    }

    #[test]
    fn config_tab_scrolls_and_notes() {
        let mut a = App { tab: Tab::Config, ..App::default() };
        a.apply(Update::Config(Some("idle_timeout = 1\n[status.idle]\ndetails = \"x\"\n".into())));
        let s = render(&a, 80, 10);
        assert!(s.contains("below the minimum") && s.contains("[status.idle]"), "{s}");
        a.scroll = u16::MAX; // clamped: the last line stays visible
        assert!(render(&a, 80, 10).contains("details"));
    }

    #[test]
    fn tiny_terminal_and_help_dont_panic() {
        let mut a = live();
        a.help = true;
        for (w, h) in [(1, 1), (10, 3), (30, 8)] {
            for t in Tab::ALL {
                a.tab = t;
                render(&a, w, h);
            }
        }
    }
}
