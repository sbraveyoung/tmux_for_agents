//! 纯渲染：输入 model，输出 ratatui 帧。本文件当前是 Task 2 的占位实现，
//! Task 4 重写为 Header/列表/详情/Footer 真布局。

use crate::tui::model::Model;
use ratatui::widgets::Paragraph;
use ratatui::Frame;

pub fn draw(f: &mut Frame, model: &Model) {
    let conn = if model.connected { "已连接" } else { "重连中…" };
    let sel = model.selected_session().map(|s| s.pane_id.as_str()).unwrap_or("-");
    let err = model.nav_error.as_deref().unwrap_or("");
    let text = format!("tfa tui — {} 个会话 · 选中 {sel} · {conn} {err} · q 退出", model.sessions.len());
    f.render_widget(Paragraph::new(text), f.area());
}
