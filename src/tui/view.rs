//! 纯渲染：输入 model 输出 ratatui 帧（Layout + widgets），无 IO。
//! 布局（spec §6/§12）：Header 1 行 + 主体 + Footer 1 行；
//! 主体 ≥100 列左右两栏(60/40)，否则上下两栏；过小则纯列表降级。

use crate::config::TuiConfig;
use crate::state::{AgentSession, ContextUsage, SessionState, Source};
use crate::tui::model::Model;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const WIDE_MIN_WIDTH: u16 = 100;
const MIN_DETAIL_WIDTH: u16 = 60;
const MIN_DETAIL_HEIGHT: u16 = 12;

// 列表列宽——list_row 与 list_header_row 共用同一组常量，表头才能保证跟数据行
// 对齐（改列宽只需改这一处，Part1 用户验收 2026-07-11）。
const ICON_COL_WIDTH: usize = 2; // 状态图标显示宽度不一（⚡💀=2 列 vs ✓…⏸⚠=1 列），固定 2 列防漂移（Part2a）
const NAME_COL_WIDTH: usize = 20;
const AGENT_COL_WIDTH: usize = 6;
const MODEL_COL_WIDTH: usize = 14;
const CTX_COL_WIDTH: usize = 4;
/// List 高亮符号（选中行前缀列）；表头缩进用同一个常量算宽度，避免两处分叉。
const LIST_HIGHLIGHT_SYMBOL: &str = "▶";

pub fn draw(f: &mut Frame, model: &Model, styles: &StateStyles) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)])
        .split(f.area());
    f.render_widget(Paragraph::new(header_line(model)), rows[0]);
    draw_body(f, rows[1], model, styles);
    f.render_widget(Paragraph::new(footer_line(model)), rows[2]);
}

fn draw_body(f: &mut Frame, area: Rect, model: &Model, styles: &StateStyles) {
    if area.width < MIN_DETAIL_WIDTH || area.height < MIN_DETAIL_HEIGHT {
        draw_list(f, area, model, styles); // 窄窗降级：纯列表（spec §12）
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
    draw_list(f, chunks[0], model, styles);
    draw_detail(f, chunks[1], model, styles);
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
    let bold = Style::default().add_modifier(Modifier::BOLD);
    // 四段计数一律 plain text，不再按状态色着色（Part2b 用户验收 2026-07-11，
    // 取代首版"与列表行/详情首行同色"的设计）；" tfa " 标题与分隔空格保留原有
    // BOLD（结构样式，不算「颜色」），burn 文本同样保持默认样式。
    let mut spans = vec![
        Span::styled(" tfa  ", bold),
        Span::raw(format!("⚡{working}")),
        Span::styled(" ", bold),
        Span::raw(format!("⏸{waiting}")),
        Span::styled(" ", bold),
        Span::raw(format!("✓{done}")),
        Span::styled(" ", bold),
        Span::raw(format!("💀{dead}")),
    ];
    let mut burn = String::new();
    for q in &model.quota {
        burn.push_str(&format!("  ·  {} {:.0} tok/min", q.provider.label(), q.burn_rate_per_min));
    }
    if !burn.is_empty() {
        spans.push(Span::raw(burn));
    }
    Line::from(spans)
}

fn footer_line(model: &Model) -> Line<'static> {
    let status = if let Some(err) = &model.nav_error {
        Span::styled(err.clone(), Style::default().fg(Color::Red))
    } else if model.connected {
        // Instant::now() 这里只用于「新鲜度」展示（距上次收到快照过了几秒），不是
        // 会话状态时长计算——不违反 spec §5「时长一律从快照时钟推算，不用本地
        // wall clock」的禁令（那条规则约束 state_since_ms 等会话内时长，必须用
        // generated_at_ms 推算避免本地/远端时钟偏移）。last_snapshot_at 是单调
        // 时钟时间戳，同进程内量「距今几秒」天然无偏移/回退问题。
        let suffix = model.last_snapshot_at.map(|t| fmt_freshness(t.elapsed().as_secs())).unwrap_or_default();
        Span::styled(format!("已连接·{suffix}"), Style::default().fg(Color::Green))
    } else {
        Span::styled("重连中…".to_string(), Style::default().fg(Color::Yellow))
    };
    Line::from(vec![Span::raw(" ↑↓/jk 选  ⏎ 跳转  q 退出 · 1s 刷新 · "), status])
}

/// Footer「已连接·」后缀——<2s 内收到的快照算「刚刚」，避免 0s/1s 这种没有信息量
/// 的抖动数字；≥2s 起显示精确秒数，方便判断 daemon 是否卡住（Part3 用户验收）。
pub fn fmt_freshness(elapsed_secs: u64) -> String {
    if elapsed_secs < 2 {
        "刚刚".to_string()
    } else {
        format!("{elapsed_secs}s前")
    }
}

fn draw_list(f: &mut Frame, area: Rect, model: &Model, styles: &StateStyles) {
    let block = Block::default().borders(Borders::ALL).title("会话");
    if model.sessions.is_empty() {
        f.render_widget(Paragraph::new("暂无活跃 agent").block(block), area);
        return;
    }
    // Block 画在外层 area，内部再拆「表头 1 行 + List」——比另起一个 Layout 拆外层
    // 更省一层嵌套：block 的上/左边框天然成为表头的上边框，无需单独画（Part1 用户验收）。
    let inner = block.inner(area);
    f.render_widget(block, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    // 表头缩进对齐 List 的高亮符号预留列——List::highlight_spacing 默认
    // WhenSelected，选中恒非空（Model 非空列表必有选中）时等价 Always，数据行
    // 内容实际从 inner.x + 高亮符号宽度开始；表头是独立 Paragraph（不经过
    // List），不手动补这段缩进就会整体左移一列，和数据行错位。
    let indent = " ".repeat(LIST_HIGHLIGHT_SYMBOL.width());
    f.render_widget(
        Paragraph::new(format!("{indent}{}", list_header_row()))
            .style(Style::default().add_modifier(Modifier::DIM | Modifier::UNDERLINED)),
        rows[0],
    );

    let items: Vec<ListItem> = model
        .sessions
        .iter()
        .map(|s| ListItem::new(list_row(s, model.generated_at_ms)).style(styles.for_state(&s.state)))
        .collect();
    let list = List::new(items)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol(LIST_HIGHLIGHT_SYMBOL);
    let mut state = ListState::default();
    state.select(model.selected_index());
    let area = rows[1];
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_detail(f: &mut Frame, area: Rect, model: &Model, styles: &StateStyles) {
    let block = Block::default().borders(Borders::ALL).title("详情");
    let Some(s) = model.selected_session() else {
        f.render_widget(Paragraph::new("暂无活跃 agent").block(block), area);
        return;
    };
    let dur = fmt_duration(model.generated_at_ms.saturating_sub(s.state_since_ms));
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("{} {}  已持续 {dur}", state_icon(&s.state), state_name(&s.state)),
        styles.for_state(&s.state),
    )));
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

/// 每状态渲染样式（列表行 + 详情首行共用同一份，spec 要求两处一致）——由 `resolve_state_styles`
/// 从 `[tui]` 配置解析出来的纯数据表，`draw`/`draw_list`/`draw_detail` 只读不算。
/// 保持 model 纯净（不依赖 ratatui）：这张表在 `commands/tui.rs` 里算好一次，经
/// `draw(f, model, styles)` 的独立参数传入，不挂在 `Model` 上（Part2c 用户验收）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StateStyles {
    pub waiting: Style,
    pub working: Style,
    pub starting: Style,
    pub done: Style,
    pub stale: Style,
    pub dead: Style,
}

impl StateStyles {
    pub fn for_state(&self, s: &SessionState) -> Style {
        match s {
            SessionState::WaitingInput { .. } => self.waiting,
            SessionState::Working => self.working,
            SessionState::Starting => self.starting,
            SessionState::Done => self.done,
            SessionState::Stale => self.stale,
            SessionState::Dead => self.dead,
        }
    }
}

/// 黑白默认（Part2b 用户验收 2026-07-11，取代首版默认彩色）：仅保留结构样式——
/// waiting 粗体（紧急度信号不应该只靠色弱用户看不到的颜色表达）、dead 灰显（灰
/// 不算「颜色」，两种模式下都保留）；其余状态一律不带 fg，沿用终端默认色。
impl Default for StateStyles {
    fn default() -> Self {
        Self {
            waiting: Style::default().add_modifier(Modifier::BOLD),
            working: Style::default(),
            starting: Style::default(),
            done: Style::default(),
            stale: Style::default(),
            dead: Style::default().fg(Color::DarkGray),
        }
    }
}

/// `[tui]` 配置 → 渲染样式表（纯函数）。`color = false`（默认）忽略 `state_colors`
/// 直接返回黑白默认——`color` 是总开关，不是「叠加」关系，黑白模式下配置覆盖不生效，
/// 语义上更简单可预测。`color = true` 时按内置调色板起步，每个状态可经
/// `state_colors` 按名覆盖（未知颜色名 → `parse_color` 返回 None → 落回调色板默认）。
pub fn resolve_state_styles(cfg: &TuiConfig) -> StateStyles {
    if !cfg.color {
        return StateStyles::default();
    }
    let pick = |key: &str, default: Option<Color>| -> Style {
        let color = cfg.state_colors.get(key).and_then(|v| parse_color(v)).or(default);
        match color {
            Some(c) => Style::default().fg(c),
            None => Style::default(),
        }
    };
    StateStyles {
        waiting: pick("waiting", Some(Color::Cyan)).add_modifier(Modifier::BOLD),
        working: pick("working", Some(Color::Green)),
        starting: pick("starting", None),
        done: pick("done", None),
        stale: pick("stale", Some(Color::Magenta)),
        dead: pick("dead", Some(Color::DarkGray)),
    }
}

/// 命名颜色 → ratatui `Color`（大小写不敏感）。未知名字返回 `None`——调用方
/// （`resolve_state_styles`）落回调色板默认，绝不因为配置写错颜色名就报错/panic。
pub fn parse_color(name: &str) -> Option<Color> {
    match name.to_ascii_lowercase().as_str() {
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "white" => Some(Color::White),
        "gray" | "grey" => Some(Color::Gray),
        "darkgray" | "darkgrey" => Some(Color::DarkGray),
        "lightred" => Some(Color::LightRed),
        "lightgreen" => Some(Color::LightGreen),
        "lightyellow" => Some(Color::LightYellow),
        "lightblue" => Some(Color::LightBlue),
        "lightmagenta" => Some(Color::LightMagenta),
        "lightcyan" => Some(Color::LightCyan),
        _ => None,
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
        "{} {} {} {} {} {}",
        pad_display(state_icon(&s.state), ICON_COL_WIDTH),
        pad_display(name, NAME_COL_WIDTH),
        pad_display(s.agent.label(), AGENT_COL_WIDTH),
        pad_display(&model, MODEL_COL_WIDTH),
        pad_display_right(&ctx, CTX_COL_WIDTH),
        state_summary(s, generated_at_ms)
    )
}

/// 列表表头——字段顺序、列宽常量与 list_row 完全同构（图标占位 / 会话 / agent /
/// 模型 / ctx / 摘要），保证表头跟数据行逐列对齐（Part1 用户验收 2026-07-11）。
/// 图标列表头刻意留空（等宽空格）：「状态」二字 4 列塞不进 2 列预算，pad_display
/// 会截成孤零零的 …——与 Starting 状态图标（同为 …）字符级撞车，表头看起来像
/// 一行 starting 数据（review 修复 2026-07-12）；空白无歧义且对齐不变。
/// 不含 List 高亮符号预留列的缩进——那是渲染层（draw_list）拼接的关注点，这里
/// 保持纯字符串、可独立单测对齐关系。
pub fn list_header_row() -> String {
    format!(
        "{} {} {} {} {} {}",
        pad_display("", ICON_COL_WIDTH),
        pad_display("会话", NAME_COL_WIDTH),
        pad_display("agent", AGENT_COL_WIDTH),
        pad_display("模型", MODEL_COL_WIDTH),
        pad_display_right("ctx", CTX_COL_WIDTH),
        "摘要"
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

/// Truncates `s` to at most `width` *display columns*, appending a 1-column
/// … when anything was cut — the display-width analogue of `truncate_chars`.
/// Char-count truncation is not enough for aligned columns: a CJK char is
/// 1 char but 2 columns, so e.g. 15 个字 = 30 列 would sail through a
/// 20-char check and blow the field.
fn truncate_display(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let budget = width.saturating_sub(1); // 给 … 留 1 列
    let mut used = 0usize;
    let mut out = String::new();
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if used + cw > budget {
            break;
        }
        used += cw;
        out.push(ch);
    }
    out.push('…');
    out
}

/// Pads `s` to exactly `width` *display columns* (terminal cell width via
/// `unicode-width`), not char count — session names may be CJK, where one
/// char occupies two columns and naive char-count math under/over-shoots
/// the column boundary in both directions. Truncates by display width
/// (with …) when over budget, left-aligns with trailing spaces when under.
pub fn pad_display(s: &str, width: usize) -> String {
    let mut out = truncate_display(s, width);
    let dw = out.width();
    if dw < width {
        out.push_str(&" ".repeat(width - dw));
    }
    out
}

/// Right-aligned sibling of `pad_display`: same display-width truncation,
/// spaces go on the left (ctx% 列右对齐；采集中 占位符超宽同样被截到列宽内）。
pub fn pad_display_right(s: &str, width: usize) -> String {
    let out = truncate_display(s, width);
    let dw = out.width();
    if dw < width {
        format!("{}{out}", " ".repeat(width - dw))
    } else {
        out
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
    use std::collections::BTreeMap;

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

    fn draw_to_buffer(model: &Model, styles: &StateStyles, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, model, styles)).unwrap();
        term.backend().buffer().clone()
    }

    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
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

    fn render_text(model: &Model, styles: &StateStyles, w: u16, h: u16) -> String {
        buffer_text(&draw_to_buffer(model, styles, w, h))
    }

    /// 定位 `marker` 在渲染文本里第一次出现的 (列, 行)——列用 display width（不是
    /// byte/char 下标）算，这样才能跟 `buf.cell((x,y))` 的列坐标系对上（CJK/emoji
    /// 内容下 byte 下标和列坐标会对不上）。
    fn find_col_row(text: &str, marker: &str) -> (u16, u16) {
        for (y, line) in text.lines().enumerate() {
            if let Some(idx) = line.find(marker) {
                return (line[..idx].width() as u16, y as u16);
            }
        }
        panic!("marker {marker:?} not found in:\n{text}");
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
    fn fmt_freshness_thresholds() {
        // Part3 用户验收：<2s 显示「刚刚」，边界 2s 起改精确秒数「Ns前」。
        assert_eq!(fmt_freshness(0), "刚刚");
        assert_eq!(fmt_freshness(1), "刚刚");
        assert_eq!(fmt_freshness(2), "2s前");
        assert_eq!(fmt_freshness(59), "59s前");
    }

    #[test]
    fn footer_shows_freshness_suffix_when_connected() {
        // model_with 内部调 apply_msg(Snapshot) 会把 last_snapshot_at 设成
        // Instant::now()；渲染发生在几微秒后，elapsed < 2s，必显示「刚刚」。
        let m = model_with(vec![], vec![], 0);
        let text = render_text(&m, &StateStyles::default(), 120, 30);
        assert!(text.contains("已连接·刚刚"), "connected footer must show freshness suffix:\n{text}");
    }

    #[test]
    fn footer_disconnected_has_no_freshness_suffix() {
        // 断连态没有「新鲜」快照可言——保持既有 重连中… 文案，不拼接后缀。
        let m = Model::new(true);
        let text = render_text(&m, &StateStyles::default(), 120, 30);
        assert!(text.contains("重连中…"), "disconnected footer unchanged:\n{text}");
        assert!(!text.contains("重连中…·"), "disconnected footer must not gain a freshness suffix:\n{text}");
    }

    #[test]
    fn pad_display_pads_by_display_width_not_char_count() {
        // CJK 名字（"会话室" 3 字 = 6 列）与 ASCII 名字（"api" 3 字 = 3 列）填到
        // 同一 display width：若按 char count 填充（如 format!("{:<20}")），CJK
        // 会因为「3 字」被当成只占 3 列而多填 3 个空格，超出列宽 3 列。
        let ascii = pad_display("api", 20);
        let cjk = pad_display("会话室", 20);
        assert_eq!(ascii.width(), 20, "ascii pad target: {ascii:?}");
        assert_eq!(cjk.width(), 20, "cjk pad target: {cjk:?}");
        assert_eq!(ascii.width(), cjk.width());
        // 截断同样必须按 display width：15 个 CJK 字 = 30 列，char-count 截断
        // （≤20 字放行）会原样通过、炸破 20 列列宽；25 个 ASCII 截到 20 字 + …
        // 也是 21 列。两类都必须收敛到恰好 20 列，且截断带 … 标记。
        let cjk_long = pad_display(&"字".repeat(15), 20);
        assert_eq!(cjk_long.width(), 20, "long cjk must be width-truncated: {cjk_long:?}");
        let ascii_long = pad_display(&"a".repeat(25), 20);
        assert_eq!(ascii_long.width(), 20, "long ascii must be width-truncated: {ascii_long:?}");
        assert!(cjk_long.contains('…') && ascii_long.contains('…'), "truncation must be marked with …");
    }

    #[test]
    fn pad_display_right_aligns_and_bounds_width() {
        assert_eq!(pad_display_right("89%", 4), " 89%");
        assert_eq!(pad_display_right("100%", 4), "100%");
        // 超宽占位符（采集中 = 6 列）同样收敛到列宽内，右对齐
        let placeholder = pad_display_right("采集中", 4);
        assert_eq!(placeholder.width(), 4, "over-wide placeholder must be bounded: {placeholder:?}");
        assert_eq!(placeholder, " 采…");
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
    fn parse_color_known_names_case_insensitive() {
        assert_eq!(parse_color("cyan"), Some(Color::Cyan));
        assert_eq!(parse_color("CYAN"), Some(Color::Cyan));
        assert_eq!(parse_color("Magenta"), Some(Color::Magenta));
        assert_eq!(parse_color("darkgray"), Some(Color::DarkGray));
        assert_eq!(parse_color("DarkGrey"), Some(Color::DarkGray));
        assert_eq!(parse_color("gray"), Some(Color::Gray));
        assert_eq!(parse_color("grey"), Some(Color::Gray));
        assert_eq!(parse_color("lightred"), Some(Color::LightRed));
        assert_eq!(parse_color("LIGHTCYAN"), Some(Color::LightCyan));
        assert_eq!(parse_color("black"), Some(Color::Black));
        assert_eq!(parse_color("white"), Some(Color::White));
    }

    #[test]
    fn parse_color_unknown_name_returns_none() {
        assert_eq!(parse_color("chartreuse"), None);
        assert_eq!(parse_color(""), None);
        assert_eq!(parse_color("#ff0000"), None);
    }

    #[test]
    fn resolve_state_styles_monochrome_default_has_no_fg_except_dead() {
        // 黑白默认（Part2b 用户验收）：仅 waiting 粗体 + dead 灰是允许的结构样式，
        // 其余状态一律不带 fg（终端默认色）。
        let styles = resolve_state_styles(&TuiConfig::default());
        assert_eq!(styles.waiting.fg, None);
        assert!(styles.waiting.add_modifier.contains(Modifier::BOLD), "waiting 粗体两种模式都生效");
        assert_eq!(styles.working.fg, None);
        assert_eq!(styles.starting.fg, None);
        assert_eq!(styles.done.fg, None);
        assert_eq!(styles.stale.fg, None);
        assert_eq!(styles.dead.fg, Some(Color::DarkGray), "dead 灰显在黑白模式下也保留（gray is fine in monochrome）");
    }

    #[test]
    fn resolve_state_styles_color_mode_uses_palette_and_overrides() {
        let mut cfg = TuiConfig { color: true, state_colors: BTreeMap::new() };
        let styles = resolve_state_styles(&cfg);
        assert_eq!(styles.waiting.fg, Some(Color::Cyan));
        assert!(styles.waiting.add_modifier.contains(Modifier::BOLD));
        assert_eq!(styles.working.fg, Some(Color::Green));
        assert_eq!(styles.starting.fg, None, "starting 调色板默认沿用终端色");
        assert_eq!(styles.done.fg, None, "done 调色板默认沿用终端色");
        assert_eq!(styles.stale.fg, Some(Color::Magenta));
        assert_eq!(styles.dead.fg, Some(Color::DarkGray));

        cfg.state_colors.insert("waiting".into(), "magenta".into());
        // 无效颜色名的回退必须用「调色板默认非 None」的状态才验得出来——用 done
        // （默认本就 None）断言 None 分不清「回退成功」和「覆盖成 None」（review
        // c5a823d 修复项）。dead 默认 Some(DarkGray)：无效名回退后必须原样保留。
        cfg.state_colors.insert("dead".into(), "not-a-real-color".into());
        let overridden = resolve_state_styles(&cfg);
        assert_eq!(overridden.waiting.fg, Some(Color::Magenta), "state_colors 覆盖调色板默认");
        assert!(overridden.waiting.add_modifier.contains(Modifier::BOLD), "覆盖颜色不影响 waiting 粗体");
        assert_eq!(overridden.dead.fg, Some(Color::DarkGray), "无效颜色名→parse_color 返回 None→回退调色板默认 DarkGray，而非清掉颜色");
    }

    #[test]
    fn monochrome_waiting_row_cell_is_bold_without_fg() {
        let styles = StateStyles::default();
        let m = model_with(
            vec![sess("%1", Some("api"), SessionState::WaitingInput { reason: "perm".into() }, 0)],
            vec![],
            0,
        );
        let buf = draw_to_buffer(&m, &styles, 120, 30);
        let text = buffer_text(&buf);
        let (x, y) = find_col_row(&text, "api");
        let cell = buf.cell((x, y)).expect("cell in bounds");
        assert!(cell.modifier.contains(Modifier::BOLD), "waiting 行必须粗体（黑白默认）");
        assert_eq!(cell.fg, Color::Reset, "黑白默认下 fg 保持终端默认（未设置任何颜色）");
    }

    #[test]
    fn color_mode_waiting_row_cell_fg_matches_resolved_palette() {
        let cfg = TuiConfig { color: true, state_colors: BTreeMap::new() };
        let styles = resolve_state_styles(&cfg);
        let m = model_with(
            vec![sess("%1", Some("api"), SessionState::WaitingInput { reason: "perm".into() }, 0)],
            vec![],
            0,
        );
        let buf = draw_to_buffer(&m, &styles, 120, 30);
        let text = buffer_text(&buf);
        let (x, y) = find_col_row(&text, "api");
        let cell = buf.cell((x, y)).expect("cell in bounds");
        assert_eq!(cell.fg, Color::Cyan, "color=true 默认调色板 waiting=Cyan");
        assert!(cell.modifier.contains(Modifier::BOLD));

        let overridden_cfg = TuiConfig { color: true, state_colors: BTreeMap::from([("waiting".to_string(), "magenta".to_string())]) };
        let overridden_styles = resolve_state_styles(&overridden_cfg);
        let buf2 = draw_to_buffer(&m, &overridden_styles, 120, 30);
        let text2 = buffer_text(&buf2);
        let (x2, y2) = find_col_row(&text2, "api");
        let cell2 = buf2.cell((x2, y2)).expect("cell in bounds");
        assert_eq!(cell2.fg, Color::Magenta, "state_colors 覆盖后 waiting 行 fg 跟着变");
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
        let text = render_text(&m, &StateStyles::default(), 120, 30);
        assert!(text.contains("⚡1 ⏸1 ✓0 💀0"), "header counts:\n{text}");
        assert!(text.contains("claude 552 tok/min"), "header burn:\n{text}");
        assert!(text.contains("agent") && text.contains("摘要"), "column header row:\n{text}");
        assert!(text.contains("api"), "list shows session name:\n{text}");
        assert!(text.contains("等 1m"), "waiting summary:\n{text}");
        assert!(text.contains("详情"), "detail pane title:\n{text}");
        assert!(text.contains("fable-5"), "model short name:\n{text}");
        assert!(text.contains("178k/200k (89%)"), "ctx in detail:\n{text}");
        assert!(text.contains("≥340k"), "quota observed with >= prefix:\n{text}");
        assert!(text.contains("q 退出"), "footer keys:\n{text}");
        assert!(text.contains("已连接"), "footer conn state:\n{text}");
    }

    /// Part1 用户验收：表头字段序列必须与 list_row 共用同一组列宽常量，否则两处
    /// 各自硬编码的宽度分叉时表头会和数据行错位。用真实构造出的两个字符串互相
    /// 定位关键字（而不是断言写死的列号），这样宽度常量改了两处不同步时才会真正
    /// 失败——写死列号的断言在两处「一起改错」时也会误报通过。
    #[test]
    fn list_header_row_aligns_with_list_row_columns() {
        let header = list_header_row();
        assert!(header.contains("会话") && header.contains("agent") && header.contains("模型"));
        assert!(header.contains("ctx") && header.contains("摘要"));
        // 图标列表头必须留空：任何截断出的 … 与 Starting 状态图标（同为 …）
        // 字符级撞车，表头会被误读成一行 starting 数据（review c5a823d 修复项）。
        assert!(
            !header.contains(state_icon(&SessionState::Starting)),
            "header icon cell collides with Starting icon: {header:?}"
        );
        let s = sess("%1", Some("api"), SessionState::Working, 0);
        let row = list_row(&s, 0);
        let header_name_col = header.split("会话").next().unwrap().width();
        let row_name_col = row.split("api").next().unwrap().width();
        assert_eq!(
            header_name_col, row_name_col,
            "会话/name 列必须对齐：header={header:?} row={row:?}"
        );
    }

    /// Part2(a) 用户验收：状态图标显示宽度不一（⚡💀=2 列，✓…⏸⚠=1 列，见
    /// unicode-width 实测），未 pad 到统一宽度时后续列会按状态整体漂移一列。
    /// 覆盖全部 6 个状态（含 spec 措辞里点名的 waiting/done），通过 TestBackend
    /// 真实渲染后在 buffer 里定位每行 session name 的起始列，断言全部相等。
    #[test]
    fn icon_padding_keeps_name_column_aligned_across_states() {
        let states: Vec<(&str, SessionState)> = vec![
            ("nWork", SessionState::Working),
            ("nWait", SessionState::WaitingInput { reason: "x".into() }),
            ("nStart", SessionState::Starting),
            ("nDone", SessionState::Done),
            ("nStale", SessionState::Stale),
            ("nDead", SessionState::Dead),
        ];
        let sessions: Vec<AgentSession> = states
            .iter()
            .enumerate()
            .map(|(i, (name, st))| sess(&format!("%{i}"), Some(name), st.clone(), 0))
            .collect();
        let m = model_with(sessions, vec![], 0);
        let text = render_text(&m, &StateStyles::default(), 120, 30);
        let mut offsets: Vec<(&str, usize)> = Vec::new();
        for (name, _) in &states {
            let line = text
                .lines()
                .find(|l| l.contains(name))
                .unwrap_or_else(|| panic!("row for {name} not rendered:\n{text}"));
            let col = line.split(name).next().unwrap().width();
            offsets.push((name, col));
        }
        let first = offsets[0].1;
        for (name, col) in &offsets {
            assert_eq!(*col, first, "{name} 的 name 列与首行错位: offsets={offsets:?}\n{text}");
        }
    }

    #[test]
    fn narrow_width_stacks_vertically_still_has_detail() {
        let m = model_with(vec![sess("%1", Some("api"), SessionState::Working, 0)], vec![], 0);
        let text = render_text(&m, &StateStyles::default(), 80, 30);
        assert!(text.contains("详情"), "vertical layout keeps detail:\n{text}");
    }

    #[test]
    fn tiny_window_degrades_to_list_only() {
        let m = model_with(vec![sess("%1", Some("api"), SessionState::Working, 0)], vec![], 0);
        let text = render_text(&m, &StateStyles::default(), 50, 10);
        assert!(!text.contains("详情"), "tiny window must hide detail:\n{text}");
        assert!(text.contains("api"), "list still renders:\n{text}");
    }

    #[test]
    fn empty_and_disconnected_states() {
        let mut m = Model::new(true);
        let text = render_text(&m, &StateStyles::default(), 120, 30);
        assert!(text.contains("暂无活跃 agent"), "empty placeholder:\n{text}");
        assert!(text.contains("重连中…"), "not yet connected:\n{text}");
        m.nav_error = Some("该会话已结束，刷新中…".into());
        let text = render_text(&m, &StateStyles::default(), 120, 30);
        assert!(text.contains("该会话已结束，刷新中…"), "nav error in footer:\n{text}");
    }
}
