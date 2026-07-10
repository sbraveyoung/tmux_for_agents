//! 纯渲染：输入 model 输出 ratatui 帧（Layout + widgets），无 IO。
//! 布局（spec §6/§12）：Header 1 行 + 主体 + Footer 1 行；
//! 主体 ≥100 列左右两栏(60/40)，否则上下两栏；过小则纯列表降级。

use crate::state::{AgentSession, ContextUsage, SessionState, Source};
use crate::tui::model::Model;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

const WIDE_MIN_WIDTH: u16 = 100;
const MIN_DETAIL_WIDTH: u16 = 60;
const MIN_DETAIL_HEIGHT: u16 = 12;

pub fn draw(f: &mut Frame, model: &Model) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)])
        .split(f.area());
    f.render_widget(Paragraph::new(header_line(model)), rows[0]);
    draw_body(f, rows[1], model);
    f.render_widget(Paragraph::new(footer_line(model)), rows[2]);
}

fn draw_body(f: &mut Frame, area: Rect, model: &Model) {
    if area.width < MIN_DETAIL_WIDTH || area.height < MIN_DETAIL_HEIGHT {
        draw_list(f, area, model); // 窄窗降级：纯列表（spec §12）
        return;
    }
    let chunks = if area.width >= WIDE_MIN_WIDTH {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(area)
    };
    draw_list(f, chunks[0], model);
    draw_detail(f, chunks[1], model);
}

fn header_line(model: &Model) -> Line<'static> {
    let (mut working, mut waiting, mut done, mut dead) = (0u32, 0u32, 0u32, 0u32);
    for s in &model.sessions {
        match &s.state {
            SessionState::Working | SessionState::Starting => working += 1,
            SessionState::WaitingInput { .. } => waiting += 1,
            SessionState::Done => done += 1,
            SessionState::Dead => dead += 1,
            SessionState::Stale => {}
        }
    }
    let mut text = format!(" tfa  ⚡{working} ⏸{waiting} ✓{done} 💀{dead}");
    for q in &model.quota {
        text.push_str(&format!("  ·  {} {:.0} tok/min", q.provider.label(), q.burn_rate_per_min));
    }
    Line::from(Span::styled(text, Style::default().add_modifier(Modifier::BOLD)))
}

fn footer_line(model: &Model) -> Line<'static> {
    let status = if let Some(err) = &model.nav_error {
        Span::styled(err.clone(), Style::default().fg(Color::Red))
    } else if model.connected {
        Span::styled("已连接".to_string(), Style::default().fg(Color::Green))
    } else {
        Span::styled("重连中…".to_string(), Style::default().fg(Color::Yellow))
    };
    Line::from(vec![Span::raw(" ↑↓/jk 选  ⏎ 跳转  q 退出 · 1s 刷新 · "), status])
}

fn draw_list(f: &mut Frame, area: Rect, model: &Model) {
    if model.sessions.is_empty() {
        let block = Block::default().borders(Borders::ALL).title("会话");
        f.render_widget(Paragraph::new("暂无活跃 agent").block(block), area);
        return;
    }
    let items: Vec<ListItem> = model
        .sessions
        .iter()
        .map(|s| {
            let style = if matches!(s.state, SessionState::Dead) {
                Style::default().fg(Color::DarkGray) // dead 行灰显（spec §6）
            } else {
                Style::default()
            };
            ListItem::new(list_row(s, model.generated_at_ms)).style(style)
        })
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("会话"))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶");
    let mut state = ListState::default();
    state.select(model.selected_index());
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_detail(f: &mut Frame, area: Rect, model: &Model) {
    let block = Block::default().borders(Borders::ALL).title("详情");
    let Some(s) = model.selected_session() else {
        f.render_widget(Paragraph::new("暂无活跃 agent").block(block), area);
        return;
    };
    let dur = fmt_duration(model.generated_at_ms.saturating_sub(s.state_since_ms));
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(format!("{} {}  已持续 {dur}", state_icon(&s.state), state_name(&s.state))));
    if let SessionState::WaitingInput { reason } = &s.state {
        lines.push(Line::from(format!("等待原因: {reason}")));
    } else if let Some(t) = &s.current_task {
        lines.push(Line::from(format!("任务: {t}")));
    }
    lines.push(Line::from(format!(
        "模型: {} · 上下文: {}",
        s.model.as_deref().unwrap_or("—"),
        fmt_ctx(s.context.as_ref())
    )));
    if let Some(t) = &s.tokens {
        lines.push(Line::from(format!(
            "tokens: in {} · out {} · cache_r {} · cache_c {} · 总 {}",
            fmt_tokens(t.input),
            fmt_tokens(t.output),
            fmt_tokens(t.cache_read),
            fmt_tokens(t.cache_creation),
            fmt_tokens(t.total)
        )));
    }
    lines.push(Line::from(format!("会话累计消耗: {}", fmt_tokens(s.consumed_tokens))));
    lines.push(Line::from(format!(
        "cwd: {} ({})",
        s.cwd.as_deref().unwrap_or("—"),
        s.git_branch.as_deref().unwrap_or("—")
    )));
    lines.push(Line::from(format!(
        "agent: {} · 来源: {} · pid: {} · pane: {}",
        s.agent.label(),
        source_label(s.source),
        s.pid.map(|p| p.to_string()).unwrap_or_else(|| "—".into()),
        s.pane_id
    )));
    if let Some(q) = model.quota.iter().find(|q| q.provider == s.agent) {
        // 本地估算无真实 limit：observed 带 ≥ 前缀诚实标注，percent 恒缺省（与 tfa list 一致）
        lines.push(Line::from(format!(
            "用量(5h窗): ≥{} · {:.1} tok/min · 本地估算",
            fmt_tokens(q.observed_tokens_this_window),
            q.burn_rate_per_min
        )));
    }
    f.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: false }), area);
}

pub fn state_icon(s: &SessionState) -> &'static str {
    match s {
        SessionState::WaitingInput { .. } => "⏸",
        SessionState::Working => "⚡",
        SessionState::Starting => "…",
        SessionState::Done => "✓",
        SessionState::Stale => "⚠",
        SessionState::Dead => "💀",
    }
}

pub fn state_name(s: &SessionState) -> &'static str {
    match s {
        SessionState::WaitingInput { .. } => "等待输入",
        SessionState::Working => "工作中",
        SessionState::Starting => "启动中",
        SessionState::Done => "完成",
        SessionState::Stale => "失联",
        SessionState::Dead => "已退出",
    }
}

pub fn list_row(s: &AgentSession, generated_at_ms: u64) -> String {
    let name = s.session_name.as_deref().unwrap_or(&s.pane_id);
    let model = s.model.as_deref().map(model_short).unwrap_or_else(|| "—".into());
    let ctx = match &s.context {
        Some(c) => match c.percent {
            Some(p) => format!("{p}%"),
            None => "采集中".into(),
        },
        None => "采集中".into(),
    };
    format!(
        "{} {} · {} · {} · {} · {}",
        state_icon(&s.state),
        truncate_chars(name, 20),
        s.agent.label(),
        model,
        ctx,
        state_summary(s, generated_at_ms)
    )
}

pub fn state_summary(s: &AgentSession, generated_at_ms: u64) -> String {
    let dur = fmt_duration(generated_at_ms.saturating_sub(s.state_since_ms));
    match &s.state {
        SessionState::WaitingInput { reason } => format!("等 {dur} · {}", truncate_chars(reason, 30)),
        SessionState::Working => truncate_chars(s.current_task.as_deref().unwrap_or("工作中"), 40),
        SessionState::Done => "完成".into(),
        SessionState::Starting => "启动中".into(),
        SessionState::Stale => "失联".into(),
        SessionState::Dead => "已退出".into(),
    }
}

pub fn fmt_duration(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        return format!("{s}s");
    }
    let m = s / 60;
    if m < 60 {
        return format!("{m}m");
    }
    format!("{}h{:02}m", m / 60, m % 60)
}

pub fn fmt_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{}k", n / 1000)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

pub fn fmt_ctx(c: Option<&ContextUsage>) -> String {
    let Some(c) = c else { return "采集中".into() };
    let used = fmt_tokens(c.used_tokens);
    let max = c.max_tokens.map(fmt_tokens).unwrap_or_else(|| "—".into());
    match c.percent {
        Some(p) => format!("{used}/{max} ({p}%)"),
        None => format!("{used}/{max}"),
    }
}

pub fn model_short(m: &str) -> String {
    truncate_chars(m.strip_prefix("claude-").unwrap_or(m), 14)
}

pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

pub fn source_label(s: Source) -> &'static str {
    match s {
        Source::Hook => "hook",
        Source::Scan => "scan",
        Source::Both => "hook+scan",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::AgentKind;
    use crate::quota::{QuotaSource, QuotaState};
    use crate::state::{AgentSession, ContextUsage, SessionState, Source, TokenTotals};
    use crate::tui::model::Model;
    use crate::tui::poll::PollMsg;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use unicode_width::UnicodeWidthStr;

    fn sess(pane: &str, name: Option<&str>, state: SessionState, since: u64) -> AgentSession {
        AgentSession {
            pane_id: pane.into(),
            agent: AgentKind::Claude,
            session_name: name.map(String::from),
            state,
            state_since_ms: since,
            current_task: Some("fix the bug".into()),
            cwd: Some("/tmp/p".into()),
            last_activity_ms: since,
            source: Source::Both,
            pid: Some(4242),
            model: Some("claude-fable-5".into()),
            context: Some(ContextUsage { used_tokens: 178_000, max_tokens: Some(200_000), percent: Some(89) }),
            tokens: Some(TokenTotals { input: 2, output: 1045, cache_read: 982_162, cache_creation: 705, total: 983_914 }),
            git_branch: Some("main".into()),
            transcript_path: None,
            agent_session_id: None,
            consumed_tokens: 12_345,
        }
    }

    fn quota(observed: u64, burn: f64) -> QuotaState {
        QuotaState {
            provider: AgentKind::Claude,
            window_5h_percent: None,
            weekly_percent: None,
            reset_at_ms: None,
            reset_estimated: true,
            observed_tokens_this_window: observed,
            burn_rate_per_min: burn,
            source: QuotaSource::LocalEstimate,
            freshness_ms: 0,
        }
    }

    fn model_with(sessions: Vec<AgentSession>, quota: Vec<QuotaState>, now: u64) -> Model {
        let mut m = Model::new(true);
        m.apply_msg(PollMsg::Snapshot { sessions, quota, generated_at_ms: now });
        m
    }

    fn render_text(model: &Model, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, model)).unwrap();
        let buf = term.backend().buffer().clone();
        let area = *buf.area();
        let mut out = String::new();
        for y in 0..area.height {
            // ratatui reserves a "continuation" cell after every 2-column-wide grapheme
            // (CJK text, some emoji) and that cell's symbol() reads back as a literal " ".
            // Skip it — same width computation ratatui itself uses for layout — so the
            // reconstructed row matches what a real terminal shows instead of gaining a
            // phantom space after every wide glyph.
            let mut x = 0u16;
            while x < area.width {
                let symbol = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
                out.push_str(symbol);
                x += symbol.width().max(1) as u16;
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(fmt_duration(5_000), "5s");
        assert_eq!(fmt_duration(21 * 60_000), "21m");
        assert_eq!(fmt_duration(3 * 3_600_000 + 5 * 60_000), "3h05m");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(178_000), "178k");
        assert_eq!(fmt_tokens(1_500_000), "1.5M");
        let ctx = ContextUsage { used_tokens: 178_000, max_tokens: Some(200_000), percent: Some(89) };
        assert_eq!(fmt_ctx(Some(&ctx)), "178k/200k (89%)");
        assert_eq!(fmt_ctx(None), "采集中");
        let no_max = ContextUsage { used_tokens: 5_000, max_tokens: None, percent: None };
        assert_eq!(fmt_ctx(Some(&no_max)), "5k/—");
        assert_eq!(model_short("claude-fable-5"), "fable-5");
        assert_eq!(model_short("gpt-5.3-codex"), "gpt-5.3-codex");
        assert_eq!(truncate_chars("字".repeat(30).as_str(), 10).chars().count(), 11); // 10 + …
        assert_eq!(source_label(Source::Both), "hook+scan");
    }

    #[test]
    fn state_summaries() {
        let now = 21 * 60_000;
        let w = sess("%1", None, SessionState::WaitingInput { reason: "needs permission".into() }, 0);
        assert_eq!(state_summary(&w, now), "等 21m · needs permission");
        let mut d = sess("%2", None, SessionState::Done, 0);
        assert_eq!(state_summary(&d, now), "完成");
        d.state = SessionState::Working;
        assert_eq!(state_summary(&d, now), "fix the bug");
        d.current_task = None;
        assert_eq!(state_summary(&d, now), "工作中");
        d.state = SessionState::Stale;
        assert_eq!(state_summary(&d, now), "失联");
        d.state = SessionState::Dead;
        assert_eq!(state_summary(&d, now), "已退出");
        d.state = SessionState::Starting;
        assert_eq!(state_summary(&d, now), "启动中");
    }

    #[test]
    fn wide_layout_header_list_detail_footer() {
        let m = model_with(
            vec![
                sess("%1", Some("api"), SessionState::WaitingInput { reason: "perm".into() }, 0),
                sess("%2", Some("web"), SessionState::Working, 0),
            ],
            vec![quota(340_000, 552.0)],
            60_000,
        );
        let text = render_text(&m, 120, 30);
        assert!(text.contains("⚡1 ⏸1 ✓0 💀0"), "header counts:\n{text}");
        assert!(text.contains("claude 552 tok/min"), "header burn:\n{text}");
        assert!(text.contains("api"), "list shows session name:\n{text}");
        assert!(text.contains("等 1m"), "waiting summary:\n{text}");
        assert!(text.contains("详情"), "detail pane title:\n{text}");
        assert!(text.contains("fable-5"), "model short name:\n{text}");
        assert!(text.contains("178k/200k (89%)"), "ctx in detail:\n{text}");
        assert!(text.contains("≥340k"), "quota observed with >= prefix:\n{text}");
        assert!(text.contains("q 退出"), "footer keys:\n{text}");
        assert!(text.contains("已连接"), "footer conn state:\n{text}");
    }

    #[test]
    fn narrow_width_stacks_vertically_still_has_detail() {
        let m = model_with(vec![sess("%1", Some("api"), SessionState::Working, 0)], vec![], 0);
        let text = render_text(&m, 80, 30);
        assert!(text.contains("详情"), "vertical layout keeps detail:\n{text}");
    }

    #[test]
    fn tiny_window_degrades_to_list_only() {
        let m = model_with(vec![sess("%1", Some("api"), SessionState::Working, 0)], vec![], 0);
        let text = render_text(&m, 50, 10);
        assert!(!text.contains("详情"), "tiny window must hide detail:\n{text}");
        assert!(text.contains("api"), "list still renders:\n{text}");
    }

    #[test]
    fn empty_and_disconnected_states() {
        let mut m = Model::new(true);
        let text = render_text(&m, 120, 30);
        assert!(text.contains("暂无活跃 agent"), "empty placeholder:\n{text}");
        assert!(text.contains("重连中…"), "not yet connected:\n{text}");
        m.nav_error = Some("该会话已结束，刷新中…".into());
        let text = render_text(&m, 120, 30);
        assert!(text.contains("该会话已结束，刷新中…"), "nav error in footer:\n{text}");
    }
}
