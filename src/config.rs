use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub notify: NotifyConfig,
    pub quota: QuotaConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    pub enabled: bool,
    pub quiet_hours: Option<QuietHours>,
    pub quiet_hours_exempt: Vec<String>,
    pub channels: Channels,
    pub triggers: Triggers,
    pub discipline: DisciplineConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct QuietHours { pub start: String, pub end: String }

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Channels { pub tmux: Toggle, pub macos: Toggle, pub http: HttpChannel }

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Toggle { pub enabled: bool }

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HttpChannel {
    pub enabled: bool,
    pub url: String,
    pub format: String,
    pub timeout_ms: u64,
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Triggers { pub waiting_input: bool, pub done: bool, pub stale: bool, pub dead: bool }

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DisciplineConfig { pub cooldown_secs: u64, pub dead_debounce_ticks: u64, pub boot_grace_secs: u64 }

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct QuotaConfig { pub burn_rate_window_mins: u64 }

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            quiet_hours: None,
            quiet_hours_exempt: vec!["dead".to_string()],
            channels: Channels::default(),
            triggers: Triggers::default(),
            discipline: DisciplineConfig::default(),
        }
    }
}
impl Default for Channels {
    fn default() -> Self { Self { tmux: Toggle { enabled: true }, macos: Toggle { enabled: true }, http: HttpChannel::default() } }
}
impl Default for Toggle { fn default() -> Self { Self { enabled: true } } }
impl Default for HttpChannel {
    fn default() -> Self { Self { enabled: false, url: String::new(), format: "bark".into(), timeout_ms: 3000, headers: BTreeMap::new() } }
}
impl Default for Triggers {
    fn default() -> Self { Self { waiting_input: true, done: false, stale: false, dead: false } }
}
impl Default for DisciplineConfig {
    fn default() -> Self { Self { cooldown_secs: 30, dead_debounce_ticks: 2, boot_grace_secs: 30 } }
}
impl Default for QuotaConfig { fn default() -> Self { Self { burn_rate_window_mins: 60 } } }

impl Config {
    /// 读 config_path()，缺失/坏值→默认，绝不 panic。
    pub fn load() -> Self {
        std::fs::read_to_string(crate::paths::config_path())
            .ok()
            .map(|s| Self::from_toml_str(&s))
            .unwrap_or_default()
    }
    /// 坏 toml → 默认（不 panic）。
    pub fn from_toml_str(s: &str) -> Self {
        toml::from_str(s).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_yields_all_defaults() {
        let c = Config::from_toml_str("");
        assert!(c.notify.enabled);
        assert!(c.notify.channels.tmux.enabled && c.notify.channels.macos.enabled);
        assert!(!c.notify.channels.http.enabled);
        assert_eq!(c.notify.channels.http.format, "bark");
        assert_eq!(c.notify.channels.http.timeout_ms, 3000);
        assert!(c.notify.triggers.waiting_input);
        assert!(!c.notify.triggers.done && !c.notify.triggers.stale && !c.notify.triggers.dead);
        assert_eq!(c.notify.discipline.cooldown_secs, 30);
        assert_eq!(c.notify.discipline.dead_debounce_ticks, 2);
        assert_eq!(c.notify.discipline.boot_grace_secs, 30);
        assert_eq!(c.notify.quiet_hours_exempt, vec!["dead".to_string()]);
        assert_eq!(c.quota.burn_rate_window_mins, 60);
        assert!(c.notify.quiet_hours.is_none());
    }

    #[test]
    fn partial_toml_overrides_only_named_fields() {
        let c = Config::from_toml_str(r#"
[notify.triggers]
done = true
stale = true
[notify.channels.http]
enabled = true
url = "http://192.168.1.9:8080/devkey"
format = "ntfy"
[notify.discipline]
cooldown_secs = 45
[quota]
burn_rate_window_mins = 30
"#);
        assert!(c.notify.triggers.done && c.notify.triggers.stale);
        assert!(c.notify.triggers.waiting_input);          // 未提及仍默认 true
        assert!(!c.notify.triggers.dead);                  // 未提及仍默认 false
        assert!(c.notify.channels.http.enabled);
        assert_eq!(c.notify.channels.http.url, "http://192.168.1.9:8080/devkey");
        assert_eq!(c.notify.channels.http.format, "ntfy");
        assert_eq!(c.notify.channels.http.timeout_ms, 3000); // 未提及仍默认
        assert_eq!(c.notify.discipline.cooldown_secs, 45);
        assert_eq!(c.notify.discipline.boot_grace_secs, 30); // 未提及仍默认
        assert_eq!(c.quota.burn_rate_window_mins, 30);
        assert!(c.notify.channels.macos.enabled);            // 整段未提仍默认
    }

    #[test]
    fn quiet_hours_parses_when_present() {
        let c = Config::from_toml_str(r#"
[notify]
quiet_hours = { start = "23:00", end = "08:00" }
"#);
        let qh = c.notify.quiet_hours.expect("quiet_hours present");
        assert_eq!(qh.start, "23:00");
        assert_eq!(qh.end, "08:00");
    }

    #[test]
    fn garbage_toml_falls_back_to_default_not_panic() {
        let c = Config::from_toml_str("this is not : valid = toml = [");
        assert!(c.notify.enabled); // 坏输入→默认，绝不 panic
    }

    #[test]
    fn partial_quiet_hours_does_not_reset_whole_config() {
        // 只给 quiet_hours.start，不给 end：QuietHours 若无 #[serde(default)]，
        // 反序列化整体失败 → from_toml_str 的 unwrap_or_default() 把整个 Config 打回默认，
        // 连同一份 toml 里设置的其它字段（这里是 triggers.done）也一起丢了。
        let c = Config::from_toml_str(r#"
[notify.triggers]
done = true
[notify.quiet_hours]
start = "23:00"
"#);
        assert!(c.notify.triggers.done, "quiet_hours 部分表不应把整个 config 打回默认");
    }
}
