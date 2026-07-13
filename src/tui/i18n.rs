//! TUI 双语文案（English + 简体中文）：纯数据表 + 语言解析纯函数，无 IO/无终端
//! （2026-07-12，发布前 i18n 任务）。`Texts` 是 `tui::view` 的唯一文案来源——
//! view 层只读取字段，不再内嵌任何语言相关字面量；`Model`/`NavError` 保持
//! 语言无关（错误只携带枚举，具体文案由 view 按 `Texts` 渲染）。

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Lang {
    En,
    Zh,
}

/// 一个 UI 字符串一个字段——`view.rs` 全部渲染文案的唯一来源。字段值全是
/// `&'static str`（`EN`/`ZH` 是编译期常量表），因此 `Texts` 本身可以 `Copy`。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Texts {
    // 两栏面板标题
    pub list_title: &'static str,
    pub detail_title: &'static str,

    // 列表表头（与 list_row 字段顺序同构：图标占位 / 会话 / agent / 模型 / ctx / 摘要）
    pub col_session: &'static str,
    pub col_agent: &'static str,
    pub col_model: &'static str,
    pub col_ctx: &'static str,
    pub col_summary: &'static str,

    // 状态名（详情栏首行 + state_name()）
    pub state_waiting: &'static str,
    pub state_working: &'static str,
    pub state_starting: &'static str,
    pub state_done: &'static str,
    pub state_stale: &'static str,
    pub state_dead: &'static str,

    // 列表行状态摘要
    pub wait_prefix: &'static str, // "{wait_prefix} {dur}" · reason
    pub probing: &'static str,     // 模型/上下文尚未采集到时的占位
    pub empty_placeholder: &'static str,

    // 详情栏字段标签
    pub label_duration: &'static str,          // 首行「已持续/for {dur}」
    pub label_reason: &'static str,
    pub label_task: &'static str,
    pub label_model: &'static str,
    pub label_context: &'static str,
    pub label_total: &'static str,              // tokens 行的「总/total」
    pub label_session_consumed: &'static str,
    pub label_source: &'static str,
    pub label_window: &'static str,
    pub label_usage_5h: &'static str,
    pub label_local_est: &'static str,
    // 真实配额详情行（2026-07-14）：与 label_local_est 对称的「真实来源」标签；
    // label_quota 是该行的前缀词（label_usage_5h 语义是「5h 窗口用量」，跟三窗口
    // 汇总行不搭）；label_reset 拼在每个窗口的本地重置时刻前面。
    pub label_quota: &'static str,
    pub label_real_source: &'static str,
    pub label_reset: &'static str,

    // Footer
    pub footer_keys: &'static str,
    pub connected: &'static str,
    pub just_now: &'static str,
    /// 拼在 "{elapsed}s" 后面的后缀——ZH 无空格直接跟「前」，EN 带前导空格
    /// " ago"，两种语言各自的自然写法都通过同一个 `format!("{elapsed}s{suffix}")`
    /// 拼出，故意不做成整句字段。
    pub ago_suffix: &'static str,
    pub reconnecting: &'static str,

    // Nav 错误（Model 只存 NavError 枚举，文案在这里按语言渲染）
    pub nav_error_not_in_tmux: &'static str,
    pub nav_error_target_gone: &'static str,
}

pub static ZH: Texts = Texts {
    list_title: "会话",
    detail_title: "详情",

    col_session: "会话",
    col_agent: "agent",
    col_model: "模型",
    col_ctx: "ctx",
    col_summary: "摘要",

    state_waiting: "等待输入",
    state_working: "工作中",
    state_starting: "启动中",
    state_done: "完成",
    state_stale: "失联",
    state_dead: "已退出",

    wait_prefix: "等",
    probing: "采集中",
    empty_placeholder: "暂无活跃 agent",

    label_duration: "已持续",
    label_reason: "等待原因",
    label_task: "任务",
    label_model: "模型",
    label_context: "上下文",
    label_total: "总",
    label_session_consumed: "会话累计消耗",
    label_source: "来源",
    label_window: "窗口",
    label_usage_5h: "用量(5h窗)",
    label_local_est: "本地估算",
    label_quota: "配额",
    label_real_source: "真实",
    label_reset: "重置",

    footer_keys: "↑↓/jk 选  ⏎ 跳转  q 退出 · 1s 刷新",
    connected: "已连接",
    just_now: "刚刚",
    ago_suffix: "前",
    reconnecting: "重连中…",

    nav_error_not_in_tmux: "非 tmux 环境，跳转不可用",
    nav_error_target_gone: "该会话已结束，刷新中…",
};

pub static EN: Texts = Texts {
    list_title: "Sessions",
    detail_title: "Details",

    col_session: "Session",
    col_agent: "agent",
    col_model: "model",
    col_ctx: "ctx",
    col_summary: "summary",

    state_waiting: "waiting input",
    state_working: "working",
    state_starting: "starting",
    state_done: "done",
    state_stale: "stale",
    state_dead: "exited",

    wait_prefix: "wait",
    probing: "probing",
    empty_placeholder: "no active agents",

    label_duration: "for",
    label_reason: "reason",
    label_task: "task",
    label_model: "model",
    label_context: "context",
    label_total: "total",
    label_session_consumed: "session consumed",
    label_source: "source",
    label_window: "window",
    label_usage_5h: "usage(5h win)",
    label_local_est: "local est",
    label_quota: "quota",
    label_real_source: "real",
    label_reset: "resets",

    footer_keys: "↑↓/jk select  ⏎ jump  q quit · 1s refresh",
    connected: "connected",
    just_now: "just now",
    ago_suffix: " ago",
    reconnecting: "reconnecting…",

    nav_error_not_in_tmux: "not inside tmux, jump unavailable",
    nav_error_target_gone: "session ended, refreshing…",
};

pub fn texts(lang: Lang) -> &'static Texts {
    match lang {
        Lang::En => &EN,
        Lang::Zh => &ZH,
    }
}

/// 纯函数：`[tui] lang` 配置 + 环境变量 → 生效语言。
///
/// - `config_lang` 显式为 `"zh"`/`"en"`（大小写不敏感）时直接生效，环境变量
///   不参与判断——用户手写的配置永远优先。
/// - 其余取值（含 `"auto"`、空串、任何拼错的值）一律落到环境变量探测：
///   `env_lang` 存在且包含 `"zh"`（大小写不敏感，覆盖 `zh_CN.UTF-8`/`zh_TW`/
///   `ZH_CN` 等常见 locale 写法）→ 中文；否则（含 `env_lang` 为 `None`）→ 英文。
pub fn resolve_lang(config_lang: &str, env_lang: Option<&str>) -> Lang {
    match config_lang.to_ascii_lowercase().as_str() {
        "zh" => Lang::Zh,
        "en" => Lang::En,
        _ => match env_lang {
            Some(s) if s.to_ascii_lowercase().contains("zh") => Lang::Zh,
            _ => Lang::En,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_zh_wins_regardless_of_env() {
        assert_eq!(resolve_lang("zh", None), Lang::Zh);
        assert_eq!(resolve_lang("zh", Some("en_US.UTF-8")), Lang::Zh, "config 显式值必须压过环境探测");
        assert_eq!(resolve_lang("ZH", Some("en_US.UTF-8")), Lang::Zh, "大小写不敏感");
    }

    #[test]
    fn config_en_wins_regardless_of_env() {
        assert_eq!(resolve_lang("en", None), Lang::En);
        assert_eq!(resolve_lang("en", Some("zh_CN.UTF-8")), Lang::En, "config 显式值必须压过环境探测");
        assert_eq!(resolve_lang("EN", Some("zh_CN.UTF-8")), Lang::En, "大小写不敏感");
    }

    #[test]
    fn auto_falls_back_to_env_detection() {
        assert_eq!(resolve_lang("auto", Some("zh_CN.UTF-8")), Lang::Zh);
        assert_eq!(resolve_lang("auto", Some("en_US.UTF-8")), Lang::En);
        assert_eq!(resolve_lang("auto", None), Lang::En, "无环境变量兜底英文");
    }

    #[test]
    fn empty_config_treated_as_auto() {
        assert_eq!(resolve_lang("", Some("zh_SG")), Lang::Zh);
        assert_eq!(resolve_lang("", None), Lang::En);
    }

    #[test]
    fn unknown_config_value_falls_back_to_env_like_auto() {
        // 拼错的值（如 "chinese"）不该硬失败，行为等价 "auto"。
        assert_eq!(resolve_lang("bogus", Some("zh_TW.UTF-8")), Lang::Zh);
        assert_eq!(resolve_lang("bogus", None), Lang::En);
    }

    #[test]
    fn env_zh_detection_is_case_insensitive_and_substring() {
        assert_eq!(resolve_lang("auto", Some("ZH_CN.UTF-8")), Lang::Zh);
        assert_eq!(resolve_lang("auto", Some("C.zh")), Lang::Zh);
    }

    #[test]
    fn texts_returns_matching_language_table() {
        assert_eq!(texts(Lang::En).connected, "connected");
        assert_eq!(texts(Lang::Zh).connected, "已连接");
        assert_eq!(texts(Lang::En) as *const Texts, &EN as *const Texts);
        assert_eq!(texts(Lang::Zh) as *const Texts, &ZH as *const Texts);
    }
}
