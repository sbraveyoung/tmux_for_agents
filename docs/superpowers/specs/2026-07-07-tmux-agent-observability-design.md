# tmux_for_agents（tfa）设计文档

日期：2026-07-07
状态：待用户审阅

## 1. 背景与问题

在 alacritty/kitty + tmux 的工作流里，多个 AI coding agent（Claude Code、Codex，
未来还有 OpenCode、Hermes 等）分散在不同 session/window/pane 中运行。用户无法及时知道：

- 哪些 agent 正在干活、哪些在等输入、哪些已完成待 review；
- 每个会话的 model、context 用量、订阅配额/limit 状态。

结果是频繁手动巡视各 pane，agent 等输入被晾着，配额被莫名耗尽。

## 2. 调研结论（2026-07-07）

### 现有工具

| 工具 | 支持 agent | 状态检测 | model/context/limit | 语言 |
|---|---|---|---|---|
| tmux-agent-sidebar (hiroppy) | Claude/Codex/OpenCode | agent hook 推事件 | 无 | Rust |
| tmux-agent-status (samleeney) | Claude/Codex/Devin+自定义 | hook 写文件+进程扫描 | 无 | Shell |
| tmux-agent-indicator (accessd) | Claude/Codex/OpenCode+自定义 | hook/notify/进程检测 | 无 | Shell |
| marmonitor (mjjo16) | Claude/Codex/Gemini | 进程+会话文件解析 | 部分（model/token） | TypeScript |
| Recon (gavraz) | 仅 Claude Code | pane 文本解析+JSONL | 有（model/context） | Rust |

相邻生态：claude-squad / ccmanager（会话编排器，要求由它们创建会话）；
ccusage / claude-code-statusline（单会话 statusline 配额监控，非跨会话仪表盘）。

### Gap

没有工具同时满足：agent 无关 + 生命周期状态 + model/context/limit + tmux 原生观测
（不接管会话创建）。这是本项目的差异化定位。

### 试用实测证据（本机）

1. **纯 hook 路线盲区**（tmux-agent-sidebar）：collector 就绪前启动的会话永远不可见
   —— 13 个存量 claude pane 全部不显示；新会话则实时可见。
2. **纯扫描路线脆弱**（marmonitor）：13 个 claude pane 只匹配到 1 个（还是 5 天前的
   陈旧会话），4 个活进程被误判为 "Unmatched, consider killing"。
3. **进程识别的坑**：claude 的 `pane_current_command` 显示为版本号（如 `2.1.202`）
   而非 `claude`，识别必须走进程树+命令行参数。

结论：事件通道与扫描通道各有致命盲区，必须双通道互补。

## 3. 已确认的需求决策

| 决策点 | 结论 |
|---|---|
| 路线 | 先试用现有工具找体感，同时自研（tmux-agent-sidebar、marmonitor 已装好试用） |
| 部署范围 | 仅本机 tmux，无跨机器聚合 |
| 首批 agent | Claude Code + Codex；opencode/hermes 等走自定义接入协议 |
| 指标 | 生命周期状态、context 剩余、订阅配额/limit、model、token 用量/成本，外加：等待时长、当前任务摘要、burn rate、异常状态、git 状态 |
| UI 形态 | 暂缓（试用后定）；数据层与 UI 解耦先行 |
| 技术栈 | Rust |
| 架构 | 方案 A：常驻 daemon + 双通道采集 + 瘦客户端 |

## 4. 总体架构

单二进制 `tfa`，子命令区分角色：

```
                    ┌─────────────────────────────────────────┐
  事件通道（推）      │            tfa daemon（常驻）             │
┌──────────────┐    │                                         │
│ claude hooks ├──→ │  ┌──────────┐    ┌─────────────────┐    │
│ codex hooks  │    │  │ 状态机    │ ←→ │ 内存状态 + 快照   │    │
└──────┬───────┘    │  │ (每pane) │    │ (崩溃可恢复)     │    │
       │            │  └────↑─────┘    └────────┬────────┘    │
  tfa hook <agent>  │       │                   │             │
  <event> (瘦转发)   │  ┌────┴────────┐          │ unix socket │
                    │  │ scanner     │          ↓             │
  扫描通道（拉）      │  │ 周期reconcile│    ┌──────────────┐    │
  · tmux list-panes │  └─────────────┘    │ 查询/订阅 API  │    │
  · 进程树           └────────────────────┴───────┬──────┴────┘
  · ~/.claude JSONL                               │
  · ~/.codex sessions                             ↓
                                     ┌────────────────────────┐
                                     │ 瘦客户端                 │
                                     │ · tfa status → 状态栏    │
                                     │ · tfa tui → 仪表盘/侧栏  │
                                     │ · tfa notify → 通知     │
                                     └────────────────────────┘
```

### 组件职责

1. **`tfa daemon`**：唯一事实来源。首个客户端/hook 访问时自动拉起（flock 防双启），
   tmux server 退出后自行退出。内存状态周期落快照，崩溃后靠快照+全量扫描恢复。
2. **`tfa hook <agent> <event>`**：agent hook 配置调用的瘦转发器。读 stdin 事件
   JSON + `TMUX_PANE` 环境变量，写 socket。daemon 不在则尝试拉起一次，失败静默
   `exit 0`，超时 100ms —— 绝不阻塞 agent。
3. **scanner**（daemon 内部周期任务）：`tmux list-panes -a` + 进程树建立
   pane↔agent 绑定；解析会话文件采资源指标；纠偏事件状态；为 hook 未报到的
   存量会话建档。
4. **瘦客户端**：一切 UI 都是 socket 消费者，形态可换可并存。

### 核心原则

- **事件赢时效，文件赢事实**：状态转换靠事件（秒级），覆盖完整性与资源指标靠扫描。
- **数据层与 UI 解耦**：UI 形态最后定，不影响 M1-M3 开工。
- **观测不接管**：不负责创建/编排会话，用户照常自由使用 tmux。

## 5. 数据模型

```rust
AgentSession {
  // 定位
  pane_id, session_name, window_index, pid, cwd,
  repo_root, branch, git_dirty,
  // 身份
  agent_kind: Claude | Codex | Custom(String),
  // 生命周期
  state: Starting | Working | WaitingInput(reason) | Done | Dead | Stale,
  state_since: Instant,          // 派生等待/运行时长（单调钟）
  current_task: String,          // 最近 prompt 摘要 + 当前工具活动
  // 资源指标
  model: String,
  context: { used, max, percent },
  tokens: { input, output, cache_read, cost_estimate },
  last_activity: Instant,
  source: Hook | Scan | Both,    // 置信度，UI 可区分显示
}

QuotaState {                      // 按 provider 聚合（非按 pane）
  provider, window_5h_percent, weekly_percent,
  reset_at, burn_rate,            // burn_rate 由 daemon 采样历史计算
  freshness, availability,        // 数据时效与可用性标记
}
```

### 状态机规则

- 事件驱动转换：`SessionStart→Starting`、`UserPromptSubmit→Working`、
  `Notification(permission/input)→WaitingInput`、`Stop→Done`、`SessionEnd→Dead`。
- 扫描纠偏：进程消失→`Dead`；会话文件时间戳与状态矛盾→标 `Stale` 并按文件事实
  纠正；hook 未报到的会话由扫描建档（`source: Scan`），状态从文件推断。

## 6. 数据源映射

### Claude Code

- **hooks**（以 Claude plugin 形式分发，`/plugin install` 一步接入）：
  `SessionStart / UserPromptSubmit / Notification / Stop / SessionEnd /
  PostToolUse`（PostToolUse 仅用于活动心跳，客户端侧节流：距上次上报 <2s 则跳过）。
- **会话 JSONL**：`~/.claude/projects/<cwd-encoded>/<session>.jsonl`，assistant
  消息自带 `usage` 与 `model` → context %、token、model 均来源于此。只从尾部
  增量读（记 offset）。
- **配额**：参考 claude-code-statusline 的 OAuth usage 接口方案。
  ⚠️ 非公开 API、凭证在 Keychain —— 实现阶段验证；失败降级为
  「本地 token 累计 + burn rate 推算」，UI 标注数据来源。
- **进程匹配**：不能依赖 `pane_current_command`（显示为版本号），走进程树+
  命令行参数识别。

### Codex

- **hooks**：`~/.codex/hooks.json`（需 config.toml 开 `codex_hooks`；本机已配好）：
  `SessionStart / UserPromptSubmit / Stop / PostToolUse`。
- **会话文件**：⚠️ 本机 `~/.codex` 为 sqlite 布局（`state_5.sqlite` 等），与公开
  文档的 JSONL rollout 描述不一致 —— 实现阶段第一件事现场探明 schema；探不出则
  仅靠 hooks + pane 活动，资源指标缺失但生命周期状态完整。

### Custom agent 接入协议（opencode/hermes 预留）

二选一即可接入：

1. agent 的 hook/wrapper 调 `tfa hook custom <name> <event>`（stdin 传 JSON）；
2. 实现 adapter：进程识别规则（必选）+ 会话文件解析器（可选）。

## 7. Socket 协议

Unix socket：`/tmp/tfa-<uid>/tfa.sock`，JSON-lines：

- `{"op":"hook","agent":"claude","event":"stop","payload":{…}}` —— hook 上报
- `{"op":"snapshot"}` —— 全量状态（状态栏用）
- `{"op":"subscribe"}` —— 长连接推送状态变更（TUI/通知器用）

客户端命令：`tfa status --format tmux`（状态栏一行）、`tfa list --json`、
`tfa tui`、`tfa notify`。

## 8. 错误处理与生命周期

| 场景 | 行为 |
|---|---|
| hook 连不上 daemon | 尝试拉起一次；100ms 超时静默 exit 0，绝不阻塞 agent |
| daemon 崩溃 | 下个客户端自动重启；快照（`~/.local/state/tfa/snapshot.json`）+ 全量扫描恢复 |
| tmux server 退出 | daemon 自行退出，不留孤儿 |
| JSONL 半写/超大 | 尾部增量读；坏行跳过不炸整源 |
| 单数据源失效 | 字段标 unavailable + 时效戳，其他源照常 |
| 双通道矛盾 | 状态标 `Stale`，按文件事实纠正，UI 可见置信度 |

## 9. 测试策略

- **状态机**：纯逻辑单测；重点覆盖事件×扫描冲突纠偏。
- **解析器**：golden tests，fixtures 取自本机真实文件（脱敏）。
- **扫描/绑定**：集成测试起隔离 tmux server（`tmux -L tfa-test`）+ 假 agent 脚本，
  验证 pane↔进程↔会话三方绑定（marmonitor 的翻车点，重点测）。
- **端到端**：起 daemon → 模拟 hook 序列 → 断言 `tfa status` 输出。

## 10. 里程碑

| 里程碑 | 内容 | 交付 |
|---|---|---|
| M1 骨架 | daemon + socket + tfa hook + claude hooks 插件 + 状态机 + `tfa status` | 状态栏回答「谁在干活/等我/完成」 |
| M2 资源指标 | scanner + JSONL 解析 + 存量会话建档 | context % / model / token |
| M3 配额与通知 | OAuth 配额 + burn rate + `tfa notify` + codex 深挖 | limit 可见、等输入主动提醒 |
| M4 UI 定型 | 结合试用体感定 TUI/侧栏形态 | 最终交互形态 |

每个里程碑独立可用，需用户验收后进入下一个。实现计划（implementation plan）
按里程碑逐个制定：本 spec 获批后先为 M1 出计划，M2-M4 在前一里程碑验收后再规划。

## 11. 风险与开放问题

| 项 | 风险/状态 | 缓解 |
|---|---|---|
| Claude 配额 API | 非公开，可能变更 | 降级为本地推算；字段标注来源 |
| Codex 数据 schema | 本机与文档不符 | 实现期现场探明；hooks 兜底 |
| Claude/Codex hook 语义演进 | 版本升级可能改事件 | adapter 层隔离；集成测试守护 |
| UI 形态 | 开放（M4 前由试用体感决定） | 数据层解耦，不阻塞 M1-M3 |
| 二进制/项目命名 | `tfa` 为暂定名 | 用户可随时改，M1 前定稿 |
