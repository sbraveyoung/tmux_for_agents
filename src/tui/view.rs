//! 纯渲染：输入 model 输出 ratatui 帧（Layout + widgets），无 IO。
//! 布局（spec §6/§12）：Header 1 行 + 主体 + Footer 1 行；
//! 主体 ≥100 列左右两栏(60/40)，否则上下两栏；过小则纯列表降级。

use crate::config::TuiConfig;
use crate::quota::{QuotaSource, QuotaState};
use crate::state::{AgentSession, ContextUsage, SessionState, Source};
use crate::tui::i18n::Texts;
use crate::tui::model::{Model, NavError};
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

pub fn draw(f: &mut Frame, model: &Model, styles: &StateStyles, texts: &Texts) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)])
        .split(f.area());
    f.render_widget(Paragraph::new(header_line(model)), rows[0]);
    draw_body(f, rows[1], model, styles, texts);
    f.render_widget(Paragraph::new(footer_line(model, texts)), rows[2]);
}

fn draw_body(f: &mut Frame, area: Rect, model: &Model, styles: &StateStyles, texts: &Texts) {
    if area.width < MIN_DETAIL_WIDTH || area.height < MIN_DETAIL_HEIGHT {
        draw_list(f, area, model, styles, texts); // 窄窗降级：纯列表（spec §12）
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
    draw_list(f, chunks[0], model, styles, texts);
    draw_detail(f, chunks[1], model, styles, texts);
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
    // 真实配额任务（2026-07-14）：RealApi 且已带 5h 百分比的条目改显示
    // "5h 62%·7d 31%"（真实数据，burn 速率不再有意义，挪去详情栏也不重复展示）；
    // 其余条目（LocalEstimate，或 RealApi 但百分比尚未就绪的过渡态）维持原样
    // burn 文案——LocalEstimate 视觉形态零变化（header_keeps_burn_for_local_estimate 钉住）。
    let mut quota_seg = String::new();
    for q in &model.quota {
        match real_5h_percent(q) {
            Some(p5) => {
                let p7 = q.weekly_percent.map(|p| p.to_string()).unwrap_or_else(|| "--".into());
                quota_seg.push_str(&format!("  ·  {} 5h {p5}%·7d {p7}%", q.provider.label()));
            }
            None => quota_seg.push_str(&format!("  ·  {} {:.0} tok/min", q.provider.label(), q.burn_rate_per_min)),
        }
    }
    if !quota_seg.is_empty() {
        spans.push(Span::raw(quota_seg));
    }
    Line::from(spans)
}

/// `source == RealApi` 且已带真实 5h 百分比 → `Some(p)`；否则（`LocalEstimate`，
/// 或防御性地 `RealApi` 但 `window_5h_percent` 尚未就绪的过渡态）一律 `None`，
/// 落回本地估算展示。header 与详情栏共用同一判定，避免两处分叉出不一致的显示态。
fn real_5h_percent(q: &QuotaState) -> Option<u8> {
    match q.source {
        QuotaSource::RealApi => q.window_5h_percent,
        QuotaSource::LocalEstimate => None,
    }
}

fn footer_line(model: &Model, texts: &Texts) -> Line<'static> {
    let status = if let Some(err) = &model.nav_error {
        // NavError 只是原因枚举——Model 语言无关，文案在这一层按 Texts 查表
        // （2026-07-12 i18n 任务：view 层是文案的唯一来源）。
        let msg = match err {
            NavError::NotInTmux => texts.nav_error_not_in_tmux,
            NavError::TargetGone => texts.nav_error_target_gone,
        };
        Span::styled(msg, Style::default().fg(Color::Red))
    } else if model.connected {
        // Instant::now() 这里只用于「新鲜度」展示（距上次收到快照过了几秒），不是
        // 会话状态时长计算——不违反 spec §5「时长一律从快照时钟推算，不用本地
        // wall clock」的禁令（那条规则约束 state_since_ms 等会话内时长，必须用
        // generated_at_ms 推算避免本地/远端时钟偏移）。last_snapshot_at 是单调
        // 时钟时间戳，同进程内量「距今几秒」天然无偏移/回退问题。
        let suffix = model.last_snapshot_at.map(|t| fmt_freshness(texts, t.elapsed().as_secs())).unwrap_or_default();
        Span::styled(format!("{}·{suffix}", texts.connected), Style::default().fg(Color::Green))
    } else {
        Span::styled(texts.reconnecting.to_string(), Style::default().fg(Color::Yellow))
    };
    Line::from(vec![Span::raw(format!(" {} · ", texts.footer_keys)), status])
}

/// Footer「已连接·」后缀——<2s 内收到的快照算「刚刚」，避免 0s/1s 这种没有信息量
/// 的抖动数字；≥2s 起显示精确秒数，方便判断 daemon 是否卡住（Part3 用户验收）。
/// 语言相关文案（"刚刚"/"just now"、"前"/" ago"）经 `Texts` 传入，本函数本身
/// 保持纯格式化逻辑不变（2026-07-12 i18n 任务）。
pub fn fmt_freshness(texts: &Texts, elapsed_secs: u64) -> String {
    if elapsed_secs < 2 {
        texts.just_now.to_string()
    } else {
        format!("{elapsed_secs}s{}", texts.ago_suffix)
    }
}

fn draw_list(f: &mut Frame, area: Rect, model: &Model, styles: &StateStyles, texts: &Texts) {
    let block = Block::default().borders(Borders::ALL).title(texts.list_title);
    if model.sessions.is_empty() {
        f.render_widget(Paragraph::new(texts.empty_placeholder).block(block), area);
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
        Paragraph::new(format!("{indent}{}", list_header_row(texts)))
            .style(Style::default().add_modifier(Modifier::DIM | Modifier::UNDERLINED)),
        rows[0],
    );

    let items: Vec<ListItem> = model
        .sessions
        .iter()
        .map(|s| ListItem::new(list_row(texts, s, model.generated_at_ms)).style(styles.for_state(&s.state)))
        .collect();
    let list = List::new(items)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol(LIST_HIGHLIGHT_SYMBOL);
    let mut state = ListState::default();
    state.select(model.selected_index());
    let area = rows[1];
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_detail(f: &mut Frame, area: Rect, model: &Model, styles: &StateStyles, texts: &Texts) {
    let block = Block::default().borders(Borders::ALL).title(texts.detail_title);
    let Some(s) = model.selected_session() else {
        f.render_widget(Paragraph::new(texts.empty_placeholder).block(block), area);
        return;
    };
    let dur = fmt_duration(model.generated_at_ms.saturating_sub(s.state_since_ms));
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("{} {}  {} {dur}", state_icon(&s.state), state_name(texts, &s.state), texts.label_duration),
        styles.for_state(&s.state),
    )));
    if let SessionState::WaitingInput { reason } = &s.state {
        lines.push(Line::from(format!("{}: {reason}", texts.label_reason)));
    } else if let Some(t) = &s.current_task {
        lines.push(Line::from(format!("{}: {t}", texts.label_task)));
    }
    lines.push(Line::from(format!(
        "{}: {} · {}: {}",
        texts.label_model,
        s.model.as_deref().unwrap_or("—"),
        texts.label_context,
        fmt_ctx(texts, s.context.as_ref())
    )));
    if let Some(t) = &s.tokens {
        lines.push(Line::from(format!(
            "tokens: in {} · out {} · cache_r {} · cache_c {} · {} {}",
            fmt_tokens(t.input),
            fmt_tokens(t.output),
            fmt_tokens(t.cache_read),
            fmt_tokens(t.cache_creation),
            texts.label_total,
            fmt_tokens(t.total)
        )));
    }
    lines.push(Line::from(format!("{}: {}", texts.label_session_consumed, fmt_tokens(s.consumed_tokens))));
    lines.push(Line::from(format!(
        "cwd: {} ({})",
        s.cwd.as_deref().unwrap_or("—"),
        s.git_branch.as_deref().unwrap_or("—")
    )));
    let window = match (s.window_index, s.pane_index) {
        (Some(w), Some(p)) => format!("{w}.{p}"),
        _ => "—".into(),
    };
    lines.push(Line::from(format!(
        "agent: {} · {}: {} · pid: {} · pane: {} · {}: {}",
        s.agent.label(),
        texts.label_source,
        source_label(s.source),
        s.pid.map(|p| p.to_string()).unwrap_or_else(|| "—".into()),
        s.pane_id,
        texts.label_window,
        window
    )));
    if let Some(q) = model.quota.iter().find(|q| q.provider == s.agent) {
        match real_5h_percent(q) {
            Some(_) => lines.push(Line::from(fmt_quota_real_line(texts, q, model.generated_at_ms))),
            None => {
                // 本地估算无真实 limit：observed 带 ≥ 前缀诚实标注，percent 恒缺省（与 tfa list 一致）
                lines.push(Line::from(format!(
                    "{}: ≥{} · {:.1} tok/min · {}",
                    texts.label_usage_5h,
                    fmt_tokens(q.observed_tokens_this_window),
                    q.burn_rate_per_min,
                    texts.label_local_est
                )));
            }
        }
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

pub fn state_name(texts: &Texts, s: &SessionState) -> &'static str {
    match s {
        SessionState::WaitingInput { .. } => texts.state_waiting,
        SessionState::Working => texts.state_working,
        SessionState::Starting => texts.state_starting,
        SessionState::Done => texts.state_done,
        SessionState::Stale => texts.state_stale,
        SessionState::Dead => texts.state_dead,
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

/// 会话列标识拆成 (基名, 坐标后缀)：坐标齐全 → (name, ":w.p")；有名无坐标 →
/// (name, " %id")；无名 → (pane_id, "")。同一 tmux session 的多个 window/pane
/// 可能各挂一个 agent——仅显示 session_name 时同名行无法区分，精确坐标（如
/// `company:3.0`）才能定位到具体 pane（Feature 目标，2026-07-12 用户验收增补）。
///
/// 拆成两段是 truncation 正确性的前提（review 2026-07-12 修复项）：列表列宽有限
/// 时只应该截基名，坐标后缀必须整段保留——`pad_display_keep_suffix` 依赖这个
/// 拆分才能把「truncate」和「保留后缀」分开处理，见该函数与 `list_row`。
pub fn display_name_parts(s: &AgentSession) -> (String, String) {
    match &s.session_name {
        Some(name) => match (s.window_index, s.pane_index) {
            (Some(w), Some(p)) => (name.clone(), format!(":{w}.{p}")),
            _ => (name.clone(), format!(" {}", s.pane_id)), // hook 先到、scanner 还没跑到这一轮
        },
        None => (s.pane_id.clone(), String::new()),
    }
}

/// `display_name_parts` 拼回完整字符串——供不需要分别处理 base/suffix 的调用方
/// （如详情栏若要整串标识）复用，语义与拆分前的旧实现完全一致。`list_row`
/// 改走 `display_name_parts` + `pad_display_keep_suffix`（截断必须只吃 base），
/// 此函数目前没有生产调用点，仅保留给未拆分 base/suffix 也无所谓的场景
/// （现有 `display_name_*` 单测继续覆盖其契约）——`#[allow(dead_code)]` 是
/// 有意保留，不是遗漏清理（review 2026-07-12 修复项）。
#[allow(dead_code)]
pub fn display_name(s: &AgentSession) -> String {
    let (base, suffix) = display_name_parts(s);
    format!("{base}{suffix}")
}

pub fn list_row(texts: &Texts, s: &AgentSession, generated_at_ms: u64) -> String {
    // pad_display_keep_suffix (不是 pad_display(&display_name(s), …)): 长名字截断
    // 只能吃 base，坐标后缀（:w.p / %id）必须整段保留，否则两个仅坐标不同的长
    // 名字会被截成同一个可见前缀，disambiguation 功能名存实亡（review 2026-07-12
    // 修复项）。
    let (name_base, name_suffix) = display_name_parts(s);
    let model = s.model.as_deref().map(model_short).unwrap_or_else(|| "—".into());
    let ctx = match &s.context {
        Some(c) => match c.percent {
            Some(p) => format!("{p}%"),
            None => texts.probing.into(),
        },
        None => texts.probing.into(),
    };
    format!(
        "{} {} {} {} {} {}",
        pad_display(state_icon(&s.state), ICON_COL_WIDTH),
        pad_display_keep_suffix(&name_base, &name_suffix, NAME_COL_WIDTH),
        pad_display(s.agent.label(), AGENT_COL_WIDTH),
        pad_display(&model, MODEL_COL_WIDTH),
        pad_display_right(&ctx, CTX_COL_WIDTH),
        state_summary(texts, s, generated_at_ms)
    )
}

/// 列表表头——字段顺序、列宽常量与 list_row 完全同构（图标占位 / 会话 / agent /
/// 模型 / ctx / 摘要），保证表头跟数据行逐列对齐（Part1 用户验收 2026-07-11）。
/// 图标列表头刻意留空（等宽空格）：「状态」二字 4 列塞不进 2 列预算，pad_display
/// 会截成孤零零的 …——与 Starting 状态图标（同为 …）字符级撞车，表头看起来像
/// 一行 starting 数据（review 修复 2026-07-12）；空白无歧义且对齐不变。
/// 不含 List 高亮符号预留列的缩进——那是渲染层（draw_list）拼接的关注点，这里
/// 保持纯字符串、可独立单测对齐关系。
pub fn list_header_row(texts: &Texts) -> String {
    format!(
        "{} {} {} {} {} {}",
        pad_display("", ICON_COL_WIDTH),
        pad_display(texts.col_session, NAME_COL_WIDTH),
        pad_display(texts.col_agent, AGENT_COL_WIDTH),
        pad_display(texts.col_model, MODEL_COL_WIDTH),
        pad_display_right(texts.col_ctx, CTX_COL_WIDTH),
        texts.col_summary
    )
}

pub fn state_summary(texts: &Texts, s: &AgentSession, generated_at_ms: u64) -> String {
    let dur = fmt_duration(generated_at_ms.saturating_sub(s.state_since_ms));
    match &s.state {
        SessionState::WaitingInput { reason } => format!("{} {dur} · {}", texts.wait_prefix, truncate_chars(reason, 30)),
        SessionState::Working => truncate_chars(s.current_task.as_deref().unwrap_or(texts.state_working), 40),
        SessionState::Done => texts.state_done.into(),
        SessionState::Starting => texts.state_starting.into(),
        SessionState::Stale => texts.state_stale.into(),
        SessionState::Dead => texts.state_dead.into(),
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

pub fn fmt_ctx(texts: &Texts, c: Option<&ContextUsage>) -> String {
    let Some(c) = c else { return texts.probing.into() };
    let used = fmt_tokens(c.used_tokens);
    let max = c.max_tokens.map(fmt_tokens).unwrap_or_else(|| "—".into());
    match c.percent {
        Some(p) => format!("{used}/{max} ({p}%)"),
        None => format!("{used}/{max}"),
    }
}

/// epoch ms → 本地时区 HH:MM（详情栏重置时刻）。libc 本地时——与 quiet_hours 同源做法
/// （`notify::discipline::local_now_min`，2026-07-14 复用同一套 `localtime_r` 手法）。
pub fn fmt_local_hm(ms: u64) -> String {
    let secs = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&secs, &mut tm);
    }
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

/// 详情栏真实配额行：三窗口百分比（5h/7d，sonnet 缺省整段省略）+ 各自本地重置
/// 时刻（`fmt_local_hm`）+ 数据来源标签 + 数据新鲜度。年龄走既有 `fmt_duration`
/// （`generated_at_ms - freshness_ms`），不新造一套「n 分钟前」文案——与「已持续」
/// 「等 21m」等既有时长展示同一套单位习惯，即使在 ZH 文案里也不翻译单位字母
/// （项目既有约定，见 `label_usage_5h` 的 ZH 值 "用量(5h窗)" 本身就混着 "5h"）。
/// 调用方已确认 `real_5h_percent(q).is_some()`，这里只管格式化。
fn fmt_quota_real_line(texts: &Texts, q: &QuotaState, generated_at_ms: u64) -> String {
    let p5 = q.window_5h_percent.map(|p| p.to_string()).unwrap_or_else(|| "--".into());
    let p7 = q.weekly_percent.map(|p| p.to_string()).unwrap_or_else(|| "--".into());
    let r5 = fmt_reset_suffix(texts, q.reset_at_ms);
    let r7 = fmt_reset_suffix(texts, q.weekly_reset_at_ms);
    let sonnet = q.weekly_sonnet_percent.map(|p| format!(" · sonnet {p}%")).unwrap_or_default();
    let age = fmt_duration(generated_at_ms.saturating_sub(q.freshness_ms));
    let quota = texts.label_quota;
    let real = texts.label_real_source;
    let ago = texts.ago_suffix;
    format!("{quota}: 5h {p5}%{r5} · 7d {p7}%{r7}{sonnet} · {real} · {age}{ago}")
}

/// `Some(ms)` → ` ({label_reset} HH:MM)`（本地时区，`fmt_local_hm`）；`None` → 空串
/// （真实来源理论上 reset_at_ms 恒有值，防御式兜底不假造时间）。
fn fmt_reset_suffix(texts: &Texts, ms: Option<u64>) -> String {
    ms.map(|ms| format!(" ({} {})", texts.label_reset, fmt_local_hm(ms))).unwrap_or_default()
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

/// Coordinate-aware sibling of `pad_display`: pads/truncates `base` + `suffix`
/// to exactly `width` *display columns*, but truncation eats only `base` —
/// `suffix` (the short ASCII `:w.p` / ` %id` coordinate tail from
/// `display_name_parts`) always survives intact. Truncating the *combined*
/// string (the old behavior — see `list_row`'s history) cuts from the right,
/// so any name longer than `width` loses its suffix first; two rows that
/// differ only by coordinates then render byte-identical, silently defeating
/// the disambiguation feature the suffix exists for (review 2026-07-12 fix).
///
/// When `base` needs truncation but display-width rounding can't fill the
/// budget exactly (a wide CJK char doesn't fit the last column), the result
/// is padded with trailing spaces after `suffix` so the total is always
/// exactly `width` — same contract as `pad_display`.
pub fn pad_display_keep_suffix(base: &str, suffix: &str, width: usize) -> String {
    let suffix_w = suffix.width();
    if suffix_w >= width {
        // Degenerate case (shouldn't happen in practice — suffix is a short
        // ASCII coordinate like ":12.3" or " %137" — but guarded so
        // `width - suffix_w` below never underflows): no room left for any
        // base at all, so fall back to padding/truncating the suffix alone.
        return pad_display(suffix, width);
    }
    let base_budget = width - suffix_w;
    let truncated_base = truncate_display(base, base_budget);
    let mut out = format!("{truncated_base}{suffix}");
    let dw = out.width();
    if dw < width {
        out.push_str(&" ".repeat(width - dw));
    }
    out
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
    use crate::tui::i18n::{EN, ZH};
    use crate::tui::model::{Model, NavError};
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
            window_index: None,
            pane_index: None,
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
            weekly_sonnet_percent: None,
            weekly_reset_at_ms: None,
        }
    }

    fn model_with(sessions: Vec<AgentSession>, quota: Vec<QuotaState>, now: u64) -> Model {
        let mut m = Model::new(true);
        m.apply_msg(PollMsg::Snapshot { sessions, quota, generated_at_ms: now });
        m
    }

    fn draw_to_buffer(model: &Model, styles: &StateStyles, texts: &Texts, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, model, styles, texts)).unwrap();
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

    fn render_text(model: &Model, styles: &StateStyles, texts: &Texts, w: u16, h: u16) -> String {
        buffer_text(&draw_to_buffer(model, styles, texts, w, h))
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
        assert_eq!(fmt_ctx(&ZH, Some(&ctx)), "178k/200k (89%)");
        assert_eq!(fmt_ctx(&ZH, None), "采集中");
        let no_max = ContextUsage { used_tokens: 5_000, max_tokens: None, percent: None };
        assert_eq!(fmt_ctx(&ZH, Some(&no_max)), "5k/—");
        assert_eq!(model_short("claude-fable-5"), "fable-5");
        assert_eq!(model_short("gpt-5.3-codex"), "gpt-5.3-codex");
        assert_eq!(truncate_chars("字".repeat(30).as_str(), 10).chars().count(), 11); // 10 + …
        assert_eq!(source_label(Source::Both), "hook+scan");
    }

    #[test]
    fn fmt_local_hm_shape() {
        // 真实机器时区各不相同，只钉「HH:MM」形状（2 位数字 + 冒号 + 2 位数字），
        // 不钉具体值——精确到分钟的本地时刻断言在 CI 上会因时区而 flaky。
        let s = fmt_local_hm(1_774_605_600_000);
        assert_eq!(s.len(), 5);
        assert!(s.as_bytes()[2] == b':' && s[..2].chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn fmt_freshness_thresholds() {
        // Part3 用户验收：<2s 显示「刚刚」，边界 2s 起改精确秒数「Ns前」。
        assert_eq!(fmt_freshness(&ZH, 0), "刚刚");
        assert_eq!(fmt_freshness(&ZH, 1), "刚刚");
        assert_eq!(fmt_freshness(&ZH, 2), "2s前");
        assert_eq!(fmt_freshness(&ZH, 59), "59s前");
        // EN 同一组阈值（2026-07-12 i18n 任务）：suffix 拼接方式不同（前导空格）
        // 但阈值语义完全一致。
        assert_eq!(fmt_freshness(&EN, 0), "just now");
        assert_eq!(fmt_freshness(&EN, 2), "2s ago");
    }

    #[test]
    fn footer_shows_freshness_suffix_when_connected() {
        // model_with 内部调 apply_msg(Snapshot) 会把 last_snapshot_at 设成
        // Instant::now()；渲染发生在几微秒后，elapsed < 2s，必显示「刚刚」。
        let m = model_with(vec![], vec![], 0);
        let text = render_text(&m, &StateStyles::default(), &ZH, 120, 30);
        assert!(text.contains("已连接·刚刚"), "connected footer must show freshness suffix:\n{text}");
    }

    #[test]
    fn footer_disconnected_has_no_freshness_suffix() {
        // 断连态没有「新鲜」快照可言——保持既有 重连中… 文案，不拼接后缀。
        let m = Model::new(true);
        let text = render_text(&m, &StateStyles::default(), &ZH, 120, 30);
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
    fn pad_display_keep_suffix_truncates_base_only_ascii() {
        // 核心修复：base+suffix 超宽时只截 base，suffix（短 ASCII 坐标）整段保留在
        // 结果末尾；两个仅 suffix 不同的长 base 必须渲染出不同结果，且都恰好落在
        // 目标列宽——disambiguation 的存在意义（review 2026-07-12 修复项）。
        let base = "pernsonal_tool_worktree-calm-ray-f3ck"; // 37 列，真实场景长度
        let a = pad_display_keep_suffix(base, ":1.0", NAME_COL_WIDTH);
        let b = pad_display_keep_suffix(base, ":12.3", NAME_COL_WIDTH);
        assert_eq!(a.width(), NAME_COL_WIDTH, "a must land on exact width: {a:?}");
        assert_eq!(b.width(), NAME_COL_WIDTH, "b must land on exact width: {b:?}");
        assert_ne!(a, b, "different suffixes on the same long base must render differently");
        assert!(a.trim_end().ends_with(":1.0"), "suffix must be visible at the end: {a:?}");
        assert!(b.trim_end().ends_with(":12.3"), "suffix must be visible at the end: {b:?}");
    }

    #[test]
    fn pad_display_keep_suffix_truncates_base_only_cjk() {
        // 同一断言换成 >10 字 CJK 基名（22 列，同样超过 20 列列宽）——CJK 宽字符
        // 在截断边界可能凑不满 base_budget（2 列字符切不开），最终必须靠补空格
        // 兜到精确列宽，不能让 CJK 场景下总宽度悄悄短 1 列。
        let base = "字".repeat(11);
        let a = pad_display_keep_suffix(&base, ":1.0", NAME_COL_WIDTH);
        let b = pad_display_keep_suffix(&base, ":2.0", NAME_COL_WIDTH);
        assert_eq!(a.width(), NAME_COL_WIDTH, "a must land on exact width: {a:?}");
        assert_eq!(b.width(), NAME_COL_WIDTH, "b must land on exact width: {b:?}");
        assert_ne!(a, b, "different suffixes on the same long cjk base must render differently");
        assert!(a.trim_end().ends_with(":1.0"), "suffix must be visible at the end: {a:?}");
        assert!(b.trim_end().ends_with(":2.0"), "suffix must be visible at the end: {b:?}");
    }

    #[test]
    fn pad_display_keep_suffix_short_name_unchanged_from_pad_display() {
        // 短名字（合并后仍 ≤ 列宽）必须跟旧的 pad_display(&(base+suffix)) 行为等价
        // ——这次重构不能改变既有短名字场景的输出（Feature 目标：向后兼容）。
        let expected = pad_display("api:3.0", NAME_COL_WIDTH);
        let actual = pad_display_keep_suffix("api", ":3.0", NAME_COL_WIDTH);
        assert_eq!(actual, expected);
        assert_eq!(actual.width(), NAME_COL_WIDTH);
    }

    #[test]
    fn pad_display_keep_suffix_degenerates_when_suffix_alone_exceeds_width() {
        // 防御性分支：suffix 自己就 ≥ width（正常不会发生——suffix 是短 ASCII 坐标，
        // 但仍需兜底）——退化为对 suffix 本身 pad_display，绝不 panic/下溢
        // （`width - suffix_w` 若不判断会在这里下溢）。
        let out = pad_display_keep_suffix("base", ":123456789.999", 6);
        assert_eq!(out, pad_display(":123456789.999", 6));
        assert_eq!(out.width(), 6);
    }

    #[test]
    fn state_summaries() {
        let now = 21 * 60_000;
        let w = sess("%1", None, SessionState::WaitingInput { reason: "needs permission".into() }, 0);
        assert_eq!(state_summary(&ZH, &w, now), "等 21m · needs permission");
        let mut d = sess("%2", None, SessionState::Done, 0);
        assert_eq!(state_summary(&ZH, &d, now), "完成");
        d.state = SessionState::Working;
        assert_eq!(state_summary(&ZH, &d, now), "fix the bug");
        d.current_task = None;
        assert_eq!(state_summary(&ZH, &d, now), "工作中");
        d.state = SessionState::Stale;
        assert_eq!(state_summary(&ZH, &d, now), "失联");
        d.state = SessionState::Dead;
        assert_eq!(state_summary(&ZH, &d, now), "已退出");
        d.state = SessionState::Starting;
        assert_eq!(state_summary(&ZH, &d, now), "启动中");
    }

    #[test]
    fn state_summaries_en() {
        // EN 同构断言（2026-07-12 i18n 任务）：不重复全部分支，只钉住 wait_prefix
        // 拼接和一个纯状态名分支，跟 ZH 版本互为「文案表没有互相污染」的证据。
        let now = 21 * 60_000;
        let w = sess("%1", None, SessionState::WaitingInput { reason: "needs permission".into() }, 0);
        assert_eq!(state_summary(&EN, &w, now), "wait 21m · needs permission");
        let d = sess("%2", None, SessionState::Dead, 0);
        assert_eq!(state_summary(&EN, &d, now), "exited");
    }

    #[test]
    fn display_name_shows_coordinates_when_full() {
        // 坐标齐全（session_name + window_index + pane_index 都有值）→ "name:w.p"，
        // 同一 session 下多个 window/pane 各挂一个 agent 也能一眼区分（Feature 目标）。
        let mut s = sess("%3", Some("company"), SessionState::Working, 0);
        s.window_index = Some(3);
        s.pane_index = Some(0);
        assert_eq!(display_name(&s), "company:3.0");
    }

    #[test]
    fn display_name_falls_back_to_name_and_pane_id_without_coordinates() {
        // 有名无坐标（hook 先到、scanner 这一轮还没跑到）→ "name %id"。
        let s = sess("%7", Some("api"), SessionState::Working, 0);
        assert_eq!(display_name(&s), "api %7");
    }

    #[test]
    fn display_name_falls_back_to_pane_id_when_unnamed() {
        // 连 session_name 都没有 → 纯 "%id"。
        let s = sess("%12", None, SessionState::Working, 0);
        assert_eq!(display_name(&s), "%12");
    }

    #[test]
    fn display_name_parts_splits_base_and_coordinate_suffix() {
        // 坐标齐全 → (session_name, ":w.p")。截断只应该吃 base，suffix 整段保留
        // 才是这个函数存在的意义（Feature 目标，review 2026-07-12 修复项）。
        let mut s = sess("%3", Some("company"), SessionState::Working, 0);
        s.window_index = Some(3);
        s.pane_index = Some(0);
        assert_eq!(display_name_parts(&s), ("company".to_string(), ":3.0".to_string()));
    }

    #[test]
    fn display_name_parts_falls_back_to_name_and_pane_id_suffix() {
        // 有名无坐标 → (name, " %id")。
        let s = sess("%7", Some("api"), SessionState::Working, 0);
        assert_eq!(display_name_parts(&s), ("api".to_string(), " %7".to_string()));
    }

    #[test]
    fn display_name_parts_falls_back_to_pane_id_base_with_empty_suffix() {
        // 无名 → (pane_id, "")。
        let s = sess("%12", None, SessionState::Working, 0);
        assert_eq!(display_name_parts(&s), ("%12".to_string(), String::new()));
    }

    #[test]
    fn list_row_keeps_distinct_coordinate_suffix_for_long_ascii_names() {
        // RED（review 2026-07-12）：用户真实 session 名常见 >20 列（如
        // `pernsonal_tool_worktree-calm-ray-f3ck`，37 列）。list_row 对拼好的
        // "name:w.p" 整串做 pad_display——truncate_display 从右边截断，两个仅
        // 坐标不同的长名字的坐标后缀会被先吃掉，截断后可见前缀完全相同，
        // disambiguation 功能名存实亡。构造两个共享同一个 37 列长基名、仅
        // window/pane 不同的会话，断言列表行必须渲染成不同的字符串、且各自的
        // 坐标后缀必须留在结果里。
        let long_name = "pernsonal_tool_worktree-calm-ray-f3ck";
        assert!(long_name.width() > NAME_COL_WIDTH, "fixture must exceed the name column width");
        let mut a = sess("%1", Some(long_name), SessionState::Working, 0);
        a.window_index = Some(1);
        a.pane_index = Some(0);
        let mut b = sess("%2", Some(long_name), SessionState::Working, 0);
        b.window_index = Some(12);
        b.pane_index = Some(3);
        let row_a = list_row(&ZH, &a, 0);
        let row_b = list_row(&ZH, &b, 0);
        assert_ne!(
            row_a, row_b,
            "two long names differing only by coordinate suffix must render differently:\na={row_a:?}\nb={row_b:?}"
        );
        assert!(row_a.contains(":1.0"), "suffix must survive truncation: {row_a:?}");
        assert!(row_b.contains(":12.3"), "suffix must survive truncation: {row_b:?}");
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
        let mut cfg = TuiConfig { color: true, state_colors: BTreeMap::new(), ..TuiConfig::default() };
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
        let buf = draw_to_buffer(&m, &styles, &ZH, 120, 30);
        let text = buffer_text(&buf);
        let (x, y) = find_col_row(&text, "api");
        let cell = buf.cell((x, y)).expect("cell in bounds");
        assert!(cell.modifier.contains(Modifier::BOLD), "waiting 行必须粗体（黑白默认）");
        assert_eq!(cell.fg, Color::Reset, "黑白默认下 fg 保持终端默认（未设置任何颜色）");
    }

    #[test]
    fn color_mode_waiting_row_cell_fg_matches_resolved_palette() {
        let cfg = TuiConfig { color: true, state_colors: BTreeMap::new(), ..TuiConfig::default() };
        let styles = resolve_state_styles(&cfg);
        let m = model_with(
            vec![sess("%1", Some("api"), SessionState::WaitingInput { reason: "perm".into() }, 0)],
            vec![],
            0,
        );
        let buf = draw_to_buffer(&m, &styles, &ZH, 120, 30);
        let text = buffer_text(&buf);
        let (x, y) = find_col_row(&text, "api");
        let cell = buf.cell((x, y)).expect("cell in bounds");
        assert_eq!(cell.fg, Color::Cyan, "color=true 默认调色板 waiting=Cyan");
        assert!(cell.modifier.contains(Modifier::BOLD));

        let overridden_cfg = TuiConfig {
            color: true,
            state_colors: BTreeMap::from([("waiting".to_string(), "magenta".to_string())]),
            ..TuiConfig::default()
        };
        let overridden_styles = resolve_state_styles(&overridden_cfg);
        let buf2 = draw_to_buffer(&m, &overridden_styles, &ZH, 120, 30);
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
        let text = render_text(&m, &StateStyles::default(), &ZH, 120, 30);
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
        // 坐标未知（本测试夹具没设 window_index/pane_index）→ 详情栏窗口字段回落 —。
        assert!(text.contains("窗口: —"), "detail pane shows em-dash when coordinates unknown:\n{text}");
    }

    /// 真实配额任务（2026-07-14）：`source == RealApi` 且 `window_5h_percent` 有值时，
    /// header 的 quota 段改显示 5h/7d 真实百分比，不再是本地估算的 burn 速率。
    #[test]
    fn header_shows_real_percents_when_source_is_real_api() {
        let mut q = quota(340_000, 552.0);
        q.window_5h_percent = Some(62);
        q.weekly_percent = Some(31);
        q.source = QuotaSource::RealApi;
        let m = model_with(vec![sess("%1", Some("api"), SessionState::Working, 0)], vec![q], 0);
        let text = render_text(&m, &StateStyles::default(), &ZH, 120, 30);
        assert!(text.contains("claude 5h 62%·7d 31%"), "real header:\n{text}");
        assert!(!text.contains("tok/min") || !text.contains("claude 552"), "burn 不再占 header：\n{text}");
    }

    /// 回归钉子：`source == LocalEstimate`（现状/默认）时 header 必须继续显示 burn
    /// 速率，视觉形态零变化——这是本任务对 LocalEstimate 路径唯一的硬约束。
    #[test]
    fn header_keeps_burn_for_local_estimate() {
        let m = model_with(vec![sess("%1", Some("api"), SessionState::Working, 0)], vec![quota(340_000, 552.0)], 0);
        let text = render_text(&m, &StateStyles::default(), &ZH, 120, 30);
        assert!(text.contains("552 tok/min"), "本地估算保持原样:\n{text}");
    }

    /// 详情栏真实态：三窗口（5h/7d/sonnet）百分比 + 数据来源标签，全部走 EN 文案表
    /// 断言（来源标签英文单词 "real" 不会出现在 ZH 文案里，这里刻意用 EN 表校验
    /// 生产渲染路径确实把 source 分支接上了，而不是仅仅新增了 i18n 字段却没接线）。
    #[test]
    fn detail_shows_three_windows_age_and_source() {
        let mut q = quota(340_000, 9.5);
        q.window_5h_percent = Some(62);
        q.weekly_percent = Some(31);
        q.weekly_sonnet_percent = Some(10);
        q.reset_at_ms = Some(1_774_605_600_000);
        q.weekly_reset_at_ms = Some(1_774_700_000_000);
        q.source = QuotaSource::RealApi;
        let m = model_with(vec![sess("%1", Some("api"), SessionState::Working, 0)], vec![q], 60_000);
        let text = render_text(&m, &StateStyles::default(), &EN, 130, 34);
        assert!(text.contains("5h 62%"), "detail 5h:\n{text}");
        assert!(text.contains("7d 31%"), "detail 7d:\n{text}");
        assert!(text.contains("sonnet 10%"), "detail sonnet:\n{text}");
        assert!(text.contains("real"), "source 标签:\n{text}");
    }

    /// 会话:窗口.面板坐标（2026-07-12 用户验收增补）：同一 session 的多个
    /// window/pane 各挂一个 agent 时，仅显示 session_name 无法区分同名行——列表
    /// 会话列改显示精确坐标 `session:window.pane`；详情栏窗口字段同步显示。
    #[test]
    fn list_row_renders_session_window_pane_coordinates() {
        let mut s = sess("%3", Some("company"), SessionState::Working, 0);
        s.window_index = Some(3);
        s.pane_index = Some(0);
        let m = model_with(vec![s], vec![], 0);
        let text = render_text(&m, &StateStyles::default(), &ZH, 120, 30);
        assert!(text.contains("company:3.0"), "list row must show session:window.pane coordinates:\n{text}");
        assert!(text.contains("窗口: 3.0"), "detail pane must show window.pane when known:\n{text}");
    }

    /// Part1 用户验收：表头字段序列必须与 list_row 共用同一组列宽常量，否则两处
    /// 各自硬编码的宽度分叉时表头会和数据行错位。用真实构造出的两个字符串互相
    /// 定位关键字（而不是断言写死的列号），这样宽度常量改了两处不同步时才会真正
    /// 失败——写死列号的断言在两处「一起改错」时也会误报通过。
    #[test]
    fn list_header_row_aligns_with_list_row_columns() {
        let header = list_header_row(&ZH);
        assert!(header.contains("会话") && header.contains("agent") && header.contains("模型"));
        assert!(header.contains("ctx") && header.contains("摘要"));
        // 图标列表头必须留空：任何截断出的 … 与 Starting 状态图标（同为 …）
        // 字符级撞车，表头会被误读成一行 starting 数据（review c5a823d 修复项）。
        assert!(
            !header.contains(state_icon(&SessionState::Starting)),
            "header icon cell collides with Starting icon: {header:?}"
        );
        let s = sess("%1", Some("api"), SessionState::Working, 0);
        let row = list_row(&ZH, &s, 0);
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
        let text = render_text(&m, &StateStyles::default(), &ZH, 120, 30);
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
        let text = render_text(&m, &StateStyles::default(), &ZH, 80, 30);
        assert!(text.contains("详情"), "vertical layout keeps detail:\n{text}");
    }

    #[test]
    fn tiny_window_degrades_to_list_only() {
        let m = model_with(vec![sess("%1", Some("api"), SessionState::Working, 0)], vec![], 0);
        let text = render_text(&m, &StateStyles::default(), &ZH, 50, 10);
        assert!(!text.contains("详情"), "tiny window must hide detail:\n{text}");
        assert!(text.contains("api"), "list still renders:\n{text}");
    }

    #[test]
    fn empty_and_disconnected_states() {
        let mut m = Model::new(true);
        let text = render_text(&m, &StateStyles::default(), &ZH, 120, 30);
        assert!(text.contains("暂无活跃 agent"), "empty placeholder:\n{text}");
        assert!(text.contains("重连中…"), "not yet connected:\n{text}");
        m.nav_error = Some(NavError::TargetGone);
        let text = render_text(&m, &StateStyles::default(), &ZH, 120, 30);
        assert!(text.contains("该会话已结束，刷新中…"), "nav error in footer:\n{text}");
    }

    #[test]
    fn en_texts_render_connected_and_session_header() {
        // Part1 用户验收补充（2026-07-12 i18n 任务）：EN 语言表至少覆盖一条
        // footer 断连态文案 + 一处表头列名，防止 EN 表只是摆设、实际渲染路径
        // 没接上。
        let m = model_with(vec![sess("%1", Some("api"), SessionState::Working, 0)], vec![], 0);
        let text = render_text(&m, &StateStyles::default(), &EN, 120, 30);
        assert!(text.contains("connected"), "EN footer must show 'connected':\n{text}");
        assert!(text.contains("Session"), "EN header must show 'Session' column cell:\n{text}");
    }
}
