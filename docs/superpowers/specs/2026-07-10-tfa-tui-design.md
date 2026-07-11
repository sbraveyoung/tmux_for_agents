# tfa TUI（tmux 内交互仪表盘）设计文档

> 里程碑：tfa TUI（内部代号 M5）。取代已废弃的 M4 web/iOS 方向（分支 `worktree-tfa-m4` 留档未合并）。
> 状态：设计定稿，经一轮对抗性去风险。
> 数据源：复用既有 daemon unix socket 的 `Response::Snapshot { sessions, quota, generated_at_ms }`。

## 1. 背景与目标

`tfa` 已上线 M1（daemon+hook+状态机+状态栏）、M2（scanner 兜底+资源指标）、M3（本地用量/burn+主动通知）。用户在 tmux 里同时开多个 AI agent（claude/codex），状态栏只显示计数（`⚡1 ⏸3 ✓1`），要看每个 agent 的模型/上下文/任务/等待时长只能敲 `tfa list`/`tfa status --format json` 读 JSON——**tmux 内没有人可读的全量渲染**。

M4 曾用 LAN web/手机方案解决「同步看」，但用户决定弃用 web 方向、改在 tmux 内直接看。本里程碑交付 `tfa tui`：一个 tmux 内的交互式全屏 TUI，**列表 + 详情两栏 + 键盘导航**，并提供 web 版没有的杀手锏——**选中某个 agent 按 Enter 直接把你带到它的 pane**（「谁在等我 → 直接过去应答」一步闭环）。

**成功标准**：
- 在 `display-popup` 弹窗或 `split-window` 侧栏里，人可读地看到全部 agent 的状态/模型/上下文%/等待时长/当前任务/burn。
- 上下键选中会话，Enter 跳到对应 pane（含多 client attach 同一 session 的正确性）。
- daemon 卡死/断连时 UI 永不冻结，键盘和退出始终响应。
- 不引入 async runtime，不破坏 tfa「hook 绝不阻塞 agent」等既有不变量。

## 2. 范围决策

**In scope**：
- 新子命令 `tfa tui`：ratatui 全屏交互 TUI。
- 每 1s 轮询 daemon 快照；列表 + 详情两栏；键盘导航（↑↓/jk 选、Enter 跳转、q/Esc/Ctrl-C 退出）。
- Enter 跳转 = `tmux switch-client + select-window + select-pane` 导航（唯一 tmux 写操作，见 §7）。
- README 提供 popup / split 两套现成键位（含 `TFA_CLIENT` 注入）。

**Out of scope（明确不做）**：
- 任何 web/HTTP/SSE/手机（M4 已废弃留档）。
- 任何 `send-keys` / 向 agent 注入输入 / 改 agent 内容——**只观测 + 导航，不接管**（导航=替你切窗口，等价你自己按 `prefix+s`，不注入按键）。
- 鼠标交互（纯键盘，见 §8）。
- 真实 OAuth 配额（属未来独立里程碑，见 M3 spec §15）。
- tfa 自动修改用户 `~/.tmux.conf`（只在 README 给键位，用户自行添加）。

## 3. 关键事实（去风险现场核实）

落 spec 前对代码与依赖做了现场核实，纠正/钉死以下前提：

1. **数据契约（`src/protocol.rs` / `src/state.rs` / `src/quota/mod.rs`）**：
   - `client::request(&Request::Snapshot) -> anyhow::Result<Response>`，返回 `Response::Snapshot { sessions: Vec<AgentSession>, quota: Vec<QuotaState>, generated_at_ms: u64 }`。
   - `AgentSession`：`pane_id`(全局唯一，如 `%37`)、`agent: AgentKind`、`session_name: Option<String>`、`state`(flatten 的 `SessionState`：`starting/working/waiting_input{reason}/done/dead/stale`)、`state_since_ms`、`current_task: Option`、`cwd: Option`、`last_activity_ms`、`source`、`pid: Option`、`model: Option`、`context: Option<ContextUsage{used_tokens,max_tokens:Option,percent:Option}>`、`tokens: Option<TokenTotals{input,output,cache_read,cache_creation,total}>`、`git_branch: Option`、`consumed_tokens: u64`。
   - `QuotaState`：`provider: AgentKind`、`window_5h_percent: Option<u8>`(本地恒 None)、`weekly_percent: Option`、`reset_at_ms: Option`、`observed_tokens_this_window: u64`、`burn_rate_per_min: f64`、`source: QuotaSource::LocalEstimate`、`freshness_ms`。
   - `AgentKind`：`Claude`/`Codex`/`Custom(String)`，序列化为 `"claude"`/`"codex"`/自定义名。
2. **client socket 有 100ms 读写超时（`src/client.rs` `IO_TIMEOUT=100ms`，且被测试钉死）**：意味着后台轮询线程单次 `request()` 天然不超过 ~100ms（+ daemon 不在时一次 50ms autospawn 重试）即返回，**卡不死**。daemon 卡住 → `request()` 返回 `Err` → 轮询下一轮重试、UI 显示「重连中」。这是 UI 永不因 IO 冻结的地基。
3. **tmux socket 约定（`src/paths.rs` `tmux_args()`）**：正常态返回**空 vec**（用默认 socket）；仅 `TFA_TMUX_SOCKET` 置位时返回 `["-L", name]`（测试隔离）。导航命令统一用 `tmux_args()` 前缀：生产走默认 socket、测试自动隔离，且与 daemon scanner 的 tmux 调用同源。**不硬编码 `-L`**。
4. **ratatui 依赖树实测（`cargo tree`）**：ratatui **0.30.2** + crossterm **0.29**（经 `ratatui-crossterm` 0.1.2），**无 tokio / 无 futures / 无 async-std**；树里的 `mio`/`signal-hook-mio` 是 crossterm **默认同步事件轮询**的底层 poll 原语（非 async executor，非 `event-stream` feature）。符合 tfa no-async 不变量。
5. **ratatui 0.28+ 自带 `ratatui::init()`/`ratatui::restore()`**：已封装「进入/退出 alternate screen + raw mode」并安装 panic hook（恢复顺序正确：先恢复终端再打印 panic 信息）。不手撸 enable/disable。

## 4. 总体架构

`tfa tui` = 瘦客户端子命令，**两线程**：

```
┌─────────────────────── tfa tui 进程 ───────────────────────┐
│  poller 线程（后台）          render 线程（主）              │
│  loop {                       loop {                         │
│    snap = client::request(     event::poll(≤150ms)          │
│      Snapshot)   ──mpsc──►      → 键盘事件                   │
│    tx.send(snap)               rx.try_recv() → 新快照        │
│    sleep(≈1s)                  仅变化时 terminal.draw()      │
│  }（严格串行，卡不死主线程）   }                             │
└────────────────────────────────────────────────────────────┘
        │ request()                         │ tmux switch-client/select-*
        ▼                                   ▼
   daemon unix socket                  当前 tmux server（$TMUX）
   （Response::Snapshot）              （Enter 导航，见 §7）
```

### 组件职责（各自单一职责、可独立测试）

- **`src/commands/tui.rs`**：子命令入口。安装 signal handler、`ratatui::init()`、起 poller 线程、跑主事件循环、退出时 `ratatui::restore()`。薄。
- **`src/tui/poll.rs`**：poller 线程体。严格串行 `request→send→sleep`；把 `Result<Snapshot>` 经 `mpsc` 送主线程（成功送快照，失败送错误态供 UI 显示「重连中」）。
- **`src/tui/model.rs`**：**纯**逻辑，无 IO/无终端。持有「最新快照 + 选中 pane_id + 连接态」，负责：列表排序（§6）、按 `pane_id` 延续选中（§6）、按键→状态迁移、时长计算（`generated_at_ms - event_ms`）。可脱离终端单元测试。
- **`src/tui/view.rs`**：**纯**渲染，输入 model 输出 ratatui 帧（Layout + widgets）。用 `TestBackend` 单测布局/格式化。
- **`src/tui/nav.rs`**：**纯**函数构造导航命令 argv（§7）+ 执行封装（`Command::output()` 捕获 stdio、查退出码）。argv 构造可脱离 tmux 单测。

### 核心原则（延续 M1/M2/M3）

- 无 async runtime；`std::thread + mpsc` 解耦（与既有架构同构）。
- client IO 有超时（复用 `client::request` 100ms）；UI 永不因 IO 冻结。
- 只观测 + 导航，绝不注入按键 / 改 agent 内容。
- 逻辑与渲染与 IO 分离，纯函数尽量多，终端相关面尽量薄。

## 5. 数据流与刷新

- poller 每 ≈1s 调 `client::request(&Request::Snapshot)`。成功 → `PollMsg::Snapshot(sessions, quota, generated_at_ms)`；失败 → `PollMsg::Disconnected`。
- 主循环 `event::poll(150ms)`：有键盘事件则处理；无论有无，`rx.try_recv()` 取最新 `PollMsg`（**只取最新、丢弃积压**，避免 UI 落后）。
- **仅在以下三种情况 `terminal.draw()`**：①按键改变了选中/视图；②收到新快照；③收到 `Event::Resize`。其余 poll 超时不重绘（防闪烁、省 CPU）。
- **时长一律从快照时钟推算**：`dur = generated_at_ms.saturating_sub(state_since_ms)` 等，**不用本地 wall clock**（避免时钟偏移）。两次快照间显示至多 1s 陈旧，可接受，不做插值。
- daemon 慢时轮询节奏自然退化为「请求耗时 + 间隔」（串行阻塞天然不并发轰炸 daemon）——这是可接受降级，不是 bug。

## 6. UI 布局与渲染

**布局（ratatui `Layout`）**：Header 1 行 + 主体 + Footer 1 行。主体宽度 **≥100 列**时左右两栏（列表左、详情右，如 60/40）；否则上下两栏（列表上、详情下）。`Resize` 事件重算。

**Header**：状态计数 `⚡<working> ⏸<waiting> ✓<done> 💀<dead>` + 各 provider burn 概览（如 `claude 552 tok/min`）。

**列表排序（纯函数，可单测）**：主键按状态紧急度——`waiting_input(0) < working(1) < starting(2) < done(3) < stale(4) < dead(5)`；`waiting_input` 组内按 `state_since_ms` 升序（**等最久的浮最顶**）；其余组内按 `last_activity_ms` 降序（最近活跃在前）。`dead` 行灰显。

**列表行**：`<状态图标> <session_name 或 pane_id> <agent 图标> <model 短名> <ctx%> <状态摘要>`。状态摘要：waiting→`等 21m` + reason 截断；working→`current_task` 截断；done→`完成`；starting→`启动中`；stale→`失联`；dead→灰显 `已退出`。空态：model 空→`—`；context 空→`采集中`。

**详情栏（选中项全指标）**：状态+时长、任务/waiting reason 全文、模型 + `context used/max (percent%)`、tokens 分项（in/out/cache_read/cache_creation/total）、cwd + git_branch + pid、agent + source、以及该 agent 对应 provider 的 quota（按 `AgentKind` 匹配：`observed_tokens_this_window`、`burn_rate_per_min`；本地估算 percent 恒缺省，显示 `≥` 前缀诚实标注，与 `tfa list` 一致）。

**Footer**：键位提示 `↑↓ 选  ⏎ 跳转  q 退出` + `(1s 刷新)` + 连接态（断连时 `重连中…`）。

**选中延续（纯函数，可单测，防跳错目标）**：选中状态存**当前选中的 `pane_id`**（非列表下标）。每次新快照后在新排序列表里按 `pane_id` 重新定位光标；若该 `pane_id` 已从快照消失，则把光标 clamp 到原位置附近的合法行（列表空则无选中）。**理由**：1s 刷新间列表会增删重排，按下标会「看着选 A、Enter 跳到 B 的 pane」——Enter 改变物理焦点，选错代价高，必须按全局唯一 ID 延续。

## 7. 跳转导航契约（杀手锏，去风险硬伤已修）

选中会话按 Enter → 把发起者带到该 agent 的 pane。这是唯一的 tmux 写操作，是**导航**（switch-client）不是 send-keys，不违反「只观测不接管」。

**契约（逐条 spec 级约束）**：

1. **显式注入发起 client，不靠 tmux 隐式推断**。多个 client attach 同一 session 时（可观测性场景常见：开两个终端窗口分别盯不同 agent），`switch-client` 不带 `-c` 会依赖 tmux 内部「当前 client」推断，从 popup/split 子进程（其本身**不是** tmux client）发起时可能切错 client 或报 `no current client`。
   - README 键位在启动 tui 时注入发起者 tty：`display-popup -e TFA_CLIENT="#{client_tty}"`（split 形态同理，见 §11）。
   - tui 内：`TFA_CLIENT` 存在 → 所有 `switch-client` 带 `-c "$TFA_CLIENT"`；缺失 → 降级为不带 `-c`（单 client 仍正常）。
2. **完整链一次性 chain**（`;` 作为独立 argv 元素传入，不走 shell、不需转义）：
   ```
   tmux <tmux_args()> switch-client [-c $TFA_CLIENT] -t <target> \; select-window -t <pane_id> \; select-pane -t <pane_id>
   ```
   因为 `select-pane` **不会**自动激活所在 window，必须显式 `select-window`。`<target>`/`<pane_id>` 由选中会话的 `pane_id`（全局唯一）解析；`session_name` 为 None 时用 `pane_id` 让 tmux 解析所属 session。
   - **⚠ 待实现期验证**：`switch-client -t <pane_id>` / `select-window -t <pane_id>` 在目标 tmux 版本矩阵上把 pane-id 解析到 session/window 的确切语义，文档表述跨版本不完全一致——这是唯一无法靠读文档钉死的点。**plan 里的确切 argv 必须在真实 tmux（含 `-L` 隔离 server）上手验后再固化**，必要时退化为「先 `display-message -p -t <pane_id> '#{session_name}\t#{window_id}'` 解析再 chain」。
3. **发完命令主动退出进程**（popup 才会关：`-E` 不因 switch-client 自动关闭）——**但先查退出码**：
   - 退出码 0 → tui 进程退出（popup / split 侧栏均随之关闭，见 §11 by-design 选退出）。
   - 非 0（pane 可能在 ≤1s 陈旧窗口里已死）→ **留在 TUI**，Footer/详情报一行错（`该会话已结束，刷新中…`），不退出、不空关 popup，等下一次快照自然纠正列表。
4. **子进程 stdio 必须捕获**：用 `Command::output()`（捕获 stdout/stderr），绝不继承——否则 tmux 打到 stderr 的任何输出会糊进 raw mode + alternate screen 把界面搞花。
5. **非 tmux 环境**（`$TMUX` 缺失）：Enter 导航禁用/隐藏并提示（`非 tmux 环境，跳转不可用`），绝不调 tmux 收到诡异报错。

## 8. 终端生命周期

- **进入/退出**：用 `ratatui::init()`（alternate screen + raw mode + 自带 panic hook：先恢复终端再打印 panic 信息）与 `ratatui::restore()`。不手撸 enable/disable。
- **两道独立防线，都要**：①主流程正常退出 / `?` 提前 return → RAII 守卫（持 `DefaultTerminal`，Drop 时 restore）+ 显式 `restore()`；②panic → `init()` 装的 panic hook。**并确认该 binary 的 Cargo profile 不是 `panic = "abort"`**（abort 下 Drop 不跑，只有 panic hook 有效）——release profile 若为体积设了 abort 需在 spec/plan 显式排除或依赖 panic hook 兜底。
- **信号兜底（`signal-hook`，纯同步、不引 tokio）**：专用线程 `Signals::new([SIGTERM, SIGHUP, SIGINT])` 阻塞等待，收到 → 走和 `q` 完全相同的「恢复终端 + 退出」路径。**缓解事实**：tui 跑在 tmux 分配的 pane/popup 的独立 pty 里，pane 被销毁时 pty 随之消亡，垃圾转义序列不泄漏到用户外层真实终端——故信号兜底是**防御性加固而非承重**，可作为可裁剪的独立小任务（split 常驻侧栏形态收益略高：pane 被 kill 前少一屏糊界面）。**SIGKILL 无法捕获**，列为已知残余风险。
- **Ctrl-C 是键盘事件不是信号**：raw mode 关闭 termios `ISIG`，Ctrl-C 不产生 SIGINT，crossterm 把它当 `KeyEvent{Char('c'), CONTROL}` 读出。事件匹配里**显式**给 `Ctrl-C` 一条和 `q`/`Esc` 相同的退出分支，否则用户按 Ctrl-C 毫无反应。（signal handler 里注册 SIGINT 是 tmux 外裸跑等边缘场景的双保险。）
- **不启用鼠标捕获**：纯键盘导航无需鼠标，`EnableMouseCapture` 是额外一类转义序列泄漏面，直接不启用。

## 9. 事件循环与并发

- **poller 线程严格串行**：`loop { snap = request(); tx.send(snap); sleep(≈1s) }`。阻塞式请求天然串行，不会并发累积——daemon 卡住时至多这一个线程阻塞（≤100ms 超时后返回 Err），主线程不受影响。**无需**「上次未返回就跳过」的额外标志（串行已保证）。
- **主/渲染线程非阻塞**：`event::poll(150ms)` 读键 + `rx.try_recv()` 取快照，二者都不阻塞。daemon 再卡，键盘响应与退出永不受影响。
- **draw-on-change**：仅 §5 三种情况重绘。
- 复用 `client::request`，**不新增 client 代码路径**、不改其 100ms 超时（该超时被测试钉死，是 hook 纪律的一部分）。

## 10. no-async 护栏

- **优先用 `ratatui::crossterm` re-export**（`use ratatui::crossterm::event::...`），不直接在 tfa `Cargo.toml` 依赖 crossterm——由 ratatui 管控 crossterm features，默认不启用会拖 futures 的 `event-stream`。（ratatui 官网有篇 async `EventStream` 教程正是「响应键盘+定时刷新」场景的诱因，后来者搜到易照抄。）若确需直接依赖 crossterm，则 `default-features = false` 只列实际所需。
- **事件循环模块顶部醒目注释**：禁止 `EventStream`/tokio，轮询模型见本 spec §9。
- **CI 硬门禁**（架构不变量保险丝）：新增一条检查 `cargo tree -e normal | grep -iE 'tokio|futures-util|async-std'` 必须为空，放进现有 CI/测试脚本。不靠 code review 记性。

## 11. 键位、驻留形态与 CLI

- **CLI**：新增 `tfa tui`（无参）。可选 `tfa tui --print-keybindings` 打印下方推荐键位（便于用户自助配置 `TFA_CLIENT`）——可选、非必需任务。
- **tfa 只做规矩的全屏 TUI 本体**，驻留形态由用户 tmux.conf 决定。README 给两套现成键位（含 `TFA_CLIENT` 注入，这是多 client 下跳对的**承重**配置）：
  - **popup（按需看，推荐）**：`bind a display-popup -E -w 90% -h 80% -e TFA_CLIENT="#{client_tty}" "tfa tui"`（`prefix+a` 弹出，`q`/Esc 关，Enter 跳转后自动关）。需 **tmux ≥ 3.2**（`display-popup` 最低版本）。
  - **侧栏常驻（split）**：`bind A split-window -h -l 40% -e TFA_CLIENT="#{client_tty}" "tfa tui"`（任意 tmux 版本可用）。split 形态下 Enter 跳转后 tui 是否退出（关掉侧栏）by design 选**退出**（与 popup 一致，语义简单）；用户要常驻可重新开。
- tfa **不自动修改** `~/.tmux.conf`，只在 README 文档化。

## 12. 错误处理与生命周期

- **daemon 不在**：`client::request` 既有 autospawn 兜底（一次 spawn + 50ms 重试）；仍失败 → `PollMsg::Disconnected` → Footer 显示 `重连中…`，下一轮重试。轮询每秒至多触发一次 autospawn，flock 单例保证不堆积。
- **快照为空**（无 agent）：列表空，详情显示占位（`暂无活跃 agent`）。
- **导航失败**：见 §7 契约 3/4/5。
- **panic / 信号 / Ctrl-C**：见 §8。
- **窗口过小**：宽/高低于阈值时降级为纯列表（隐藏详情栏），不 panic。（实现注记 2026-07-11：原文还要求「并在 Footer 提示」，实现省略了该提示——窄窗下 Footer 空间本就紧张，键位提示保持原样；终审提出、用户确认接受现状。）

## 13. 依赖选型

- **新增**：`ratatui = "0.30"`（含默认 crossterm backend）；`signal-hook`（同步信号兜底，无 async）。
- **不新增直接 crossterm 依赖**（走 `ratatui::crossterm` re-export）。
- 复用既有：`serde`/`serde_json`（快照反序列化）、`anyhow`、`libc`（如信号/pty 需要）、`client`/`protocol`/`state`/`quota`/`paths` 模块。
- 全部依赖经 §3.4 / §10 核实无 tokio/futures/async-std。

## 14. 测试策略

- **纯逻辑单测（无终端、无 tmux）**：
  - 列表排序：构造混合状态的 `Vec<AgentSession>`，断言 waiting 浮顶、组内次序、dead 垫底。
  - 选中延续：给定旧选中 `pane_id` + 新快照（该 pane 消失/重排），断言光标重定位/clamp 正确。
  - 导航 argv 构造（`nav.rs` 纯函数）：给定选中会话 + `TFA_CLIENT` 有/无 + `tmux_args()` 空/非空，断言生成的 argv 逐元素正确（含 `;` 分隔、`-c` 有无、`-L` 前缀）。
  - 时长/格式化：`generated_at_ms - state_since_ms` → `等 21m`；context `178000/200000` → `178k/200k (89%)`；空态 `—`/`采集中`。
- **渲染单测**：ratatui `TestBackend` 渲染已知 model 到离屏 buffer，断言关键单元格/行内容（Header 计数、某行含 model 与 ctx%、详情字段）。
- **no-async 门禁测试/脚本**：§10 的 `cargo tree | grep` 断言为空。
- **真机验收（需活 tmux + attached client，人工）**：popup 弹出/关闭、Enter 跳转到正确 pane（含多 client attach 同一 session）、daemon 手动 kill 时 UI 不冻结显示重连、窄窗降级、q/Ctrl-C/Esc 退出后终端干净、panic 注入后终端可恢复。对应 §7.2 的 tmux 版本矩阵手验也在此阶段完成。

## 15. 任务分解预览（约 6–7 个）

1. **CLI 骨架 + 依赖 + 终端生命周期**：加 `ratatui`/`signal-hook`；`tfa tui` 子命令；`ratatui::init/restore` + RAII 守卫 + panic hook 验证 + `panic=abort` 排除；最小「起屏 → q/Ctrl-C/Esc 退出、终端干净」闭环（暂空数据）。含 no-async 门禁。
2. **poller 线程 + model**：`poll.rs` 串行轮询经 mpsc 送 `PollMsg`；`model.rs` 持快照/连接态；主循环非阻塞消费 + draw-on-change。真数据进来但先纯列表。
3. **列表排序 + 选中延续**：排序纯函数 + 按 `pane_id` 延续（含单测）。
4. **view 渲染**：Header/列表行/详情栏/Footer 两栏布局 + 窄窗降级 + 空态；`TestBackend` 单测。
5. **nav 导航**：`nav.rs` argv 构造（单测）+ `Command::output()` 执行 + 退出码分支（§7）+ 非 tmux 降级。**含 §7.2 tmux 版本矩阵手验**。
6. **signal-hook 兜底 + 键位文档**：SIGTERM/SIGHUP/SIGINT 专用线程走退出路径（可裁剪）；README 两套键位（含 `TFA_CLIENT`）+ 可选 `--print-keybindings`。
7.（如需）**e2e + 真机验收整理**：no-async 门禁纳入 CI、文档收尾、真机验收清单。

## 16. 已知限制与风险

- **嵌套 tmux**（SSH 进远程机器又开 tmux）：`$TMUX`/`TFA_CLIENT` 归属可能错乱，**不保证** Enter 跳转正确性——列为已知限制。
- **SIGKILL** 无法捕获，被 `kill -9` 时终端恢复不了（残余风险，独立 pty 场景外层终端不受影响）。
- **多 client 且未配 `TFA_CLIENT`**：降级为默认推断，可能切错 client——README 强调配 `TFA_CLIENT` 的键位。
- **多实例并发**（同时开 popup + split）：各自独立每秒轮询、各自独立操作，良性竞态（后写者赢），无需加锁协调。
- **§7.2 的 tmux 目标解析语义**跨版本差异：必须在 plan/实现期真机手验后固化 argv，不信任设计期结论直接实现。
- ratatui 0.30 是较新的模块化重构版（0.29→0.30 拆分了 crates），实现期注意 API 以 0.30 为准，勿照抄 0.29 教程。
