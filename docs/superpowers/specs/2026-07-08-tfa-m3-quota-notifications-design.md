# tfa M3 配额与通知 设计文档

日期：2026-07-08
状态：待用户审阅

> 本 spec 已吸收一轮多 agent 去风险评审（3 份外部研究 + 4 份对抗评审，37 项发现去重后 4 CRITICAL + 13 IMPORTANT）。关键前提错误已在设计层纠正，见 §3。

## 1. 背景与目标

M1（hook 事件通道 + 状态机 + `tfa status` 状态栏）、M2（scanner 兜底通道 + 资源指标：claude context%/model/token、codex tokens_used）已上线合并到 master。

M3 兑现用户最初痛点里尚未解决的那半：**「agent 等输入被晾着」——不盯着 tmux 也能被主动提醒**（含手机推送），外加本地用量/burn rate 可见。

## 2. 范围决策

| 决策点 | 结论 |
|---|---|
| 配额来源 | **M3 只做 LocalEstimate（本地推算）**。真实 OAuth 配额 API（RealApiProvider）+ Keychain 访问 + `quota_alert` 主动告警**全部推后**到未来「真实配额」里程碑。**M3 零凭证访问、零非公开端点依赖。** |
| 通知架构 | daemon 内直接派发（**非** subscribe 客户端；subscribe 留 M4） |
| 通知通道 | 三个，各可单独开关：macOS（terminal-notifier 优先 / osascript 兜底）｜ tmux（display-message）｜ HTTP 推送（Bark/ntfy 自建 server，手机 alert） |
| M3 主动通知触发 | `waiting_input` / `done` / `stale` / `dead`（原 `abnormal` 拆成 stale+dead）。`quota_alert` 随真实 API 一起推后 |
| codex | 白得 notify + burn rate（agent 无关）；**不深挖 rollout 文件** |
| burn rate / 本地用量 | 本地算出 burn rate + 本 5h 窗口观测用量，在 `tfa list` / `status --format json` **显示**，M3 不主动告警 |
| 配置 | 首次引入用户可编辑 config 文件 `~/.config/tfa/config.toml` |

## 3. 关键代码事实（去风险现场核实，纠正原设计前提）

实现前必须以这些**已核实**的事实为准，不得沿用早期口头设计里的错误前提：

1. **🔴 `session.tokens.total` 不是累计量（C2）**。`src/sources/claude_jsonl.rs::read_update` 只保留窗口内**最后一条** assistant 消息的**单次** usage，`total = input+output+cache_read+cache_creation`，实测被 `cache_read` 主导（真实 fixture 982162/983914）。**直接拿它算 burn rate 会算出负数与假尖峰。** M3 必须先建一个真正单调递增的 per-session `consumed` 计数器（§7）。
2. **`StateStore` 只序列化 `sessions` 字段**（`src/state.rs`）；`AgentSession` 新字段已守 `#[serde(default)]`。新增的易失态（QuotaState / burn 缓冲 / 冷却）**不进快照**（§9）。
3. **daemon 是 setsid 用户态子进程**（`src/client.rs` spawn 只 `libc::setsid()` 脱离控制终端，不切用户、不走 launchd），继承 GUI 登录会话——**不是 root-无-GUI 的 TCC 死结**，可直接派发通知，无需 LaunchAgent 中转（A3 核实）。
4. **状态写点散落 5 处**（`apply` / `reconcile_liveness` 复活+标Dead / `stale_sweep` / `set_metrics` 的 Stale→Working 复活 / `fresh_session` 重建）。逐处接线通知必漏——用 **tick 边界状态快照 diff** 单点算净边沿（§6）。
5. **`SessionState` 无 `PartialEq`**，且 `state_since_ms` 在 `apply()` 对同态事件也重置——**不能当边沿信号**（§6）。

## 4. 总体架构

```
                  ┌──────────────────── tfa daemon（常驻）─────────────────────┐
  hook 事件 ─────→ │  ┌──────────┐   tick/apply 边界                              │
  scan tick ─────→ │  │ 状态机    │ ──── 快照 diff → 净边沿 ──┐                    │
                  │  │StateStore │                          ↓                    │
                  │  └────┬─────┘              ┌────────────────────────┐        │
                  │       │ 每轮采 consumed     │ 通知纪律（边沿冷却/       │        │
                  │       ↓                    │ boot-grace/generation/  │        │
                  │  ┌──────────────┐          │ dead去抖）→ NotifyEvent  │        │
                  │  │ burn 采样器   │          └───────────┬────────────┘        │
                  │  │(provider聚合, │                      │ mpsc 入队（锁外，非阻塞）│
                  │  │ 只加正delta)  │          ┌───────────↓────────────┐        │
                  │  └──────┬───────┘          │ notifier 独立线程        │        │
                  │         │ 写               │ (硬超时, ureq)          │        │
                  │  ┌──────↓────────┐         │  ├─ macOS(term-notif/    │        │
                  │  │ QuotaCache    │         │  │   osascript)          │        │
                  │  │ Arc<Mutex>    │←读 list/ │  ├─ tmux(display-message)│        │
                  │  │ (不进快照)     │  status │  └─ HTTP(Bark/ntfy POST) │        │
                  │  └───────────────┘         └────────────────────────┘        │
                  └────────────────────────────────────────────────────────────┘
                  config: ~/.config/tfa/config.toml（启动读，缺失全默认）
```

### 组件职责

1. **config 模块**：解析 `~/.config/tfa/config.toml`，缺失/坏值全用默认。路径经 `TFA_CONFIG_PATH` 覆写（测试用），否则 `~/.config/tfa/config.toml`（不动态读 `XDG_CONFIG_HOME`，与仓库 `state_dir`/`projects_dir` 手法一致）。
2. **burn 采样器**（daemon 内，独立于 StateStore 锁）：每 scan 轮把各会话的**单调 consumed** 采一份，按 provider 聚合成单调累加序列，存环形缓冲，算 burn rate，写入独立 `Arc<Mutex<QuotaCache>>`。
3. **QuotaCache**（独立 `Arc<Mutex>`，**不参与 serde、不进快照**）：存 per-provider `QuotaState`（M3 lite）。`tfa list`/`status` 只读它。
4. **通知纪律**（tick/apply 边界）：快照 diff 算净边沿 → 过冷却/boot-grace/去抖过滤 → 产 `NotifyEvent`，锁外经 mpsc 入队。
5. **notifier 独立线程**：唯一消费 NotifyEvent 队列的线程，串行派发到三通道，每次通道 IO 带**硬超时**（绝不阻塞 daemon 其他线程）。
6. **通道**：macOS / tmux / HTTP，纯 dispatch，封装成 `tfa notify send`（daemon 调）+ `tfa notify test`（用户手验）。

### 核心原则（延续 M1/M2）

- **`tfa hook` 绝不阻塞 agent**：所有通知/采样 IO 在 daemon 内；hook 客户端路径（`src/client.rs`、`src/commands/hook.rs`）一行通知/配额代码都不得出现（§6 护栏）。
- **无 async runtime**：std::thread + Arc<Mutex>；HTTP 用 **ureq**（纯阻塞、自带超时、不拉 tokio），**禁 reqwest**（M1）。
- **daemon 内 IO 绝不持 StateStore 锁跨 IO**（M2 纪律）。
- **快照只做加法**：新态一律不进 StateStore（§9）。

## 5. 数据模型

```rust
// 单调 consumed 计数器（burn rate 唯一可信输入；tokens.total 只给上下文%显示，绝不喂 burn）
// 按 agent_session_id 键，累加每条新 assistant 行的 delta（output_tokens，或 input+output，
// 但排除 cache_read 避免重复计上下文），claude 与 codex(tokens_used) 统一到此口径。
SessionConsumed { session_id: String, provider: Provider, cumulative: u64, last_seen_ms: u64 }

// M3 lite：source 恒为 LocalEstimate，percent 恒 None（§7 诚实性）
QuotaState {
  provider,
  window_5h_percent: Option<u8>,   // M3 恒 None（无真实 limit）
  weekly_percent: Option<u8>,      // M3 恒 None
  reset_at_ms: Option<u64>,        // 5h 滚动窗口边界（估算，见 §7）
  reset_estimated: bool,           // M3 恒 true
  observed_tokens_this_window: u64,// ≥ 下界，命名/前缀明示「observed，非剩余」
  burn_rate: f64,                  // tokens/min，只来自 consumed 序列
  source: LocalEstimate,           // M3 唯一取值
  freshness_ms: u64,
  availability,
}

NotifyEvent {
  session_id: String,              // 冷却/去重键（非裸 pane_id）
  pane_id: String,
  kind: WaitingInput | Done | Stale | Dead,   // quota_alert 推后，M3 无此 kind
  title: String,
  body: String,
}
```

## 6. 通知纪律（吸收 I1–I6, I12）

**边沿检测——tick/apply 边界快照 diff（不逐 mutation 接线）**：
- 进入 tick()/apply() 前记 `old: pane→触发态判别式`；全部 mutation 跑完后与 `new` 对比，一次性算净边沿。天然覆盖 5 处写点，并顺带消除 tick 内抖动（如 stale_sweep 标 Stale 又被 set_metrics un-Stale，净状态没变则不发）。
- 「触发态判别式」= 忽略 `WaitingInput{reason}` 里 reason 的状态判别（手写忽略 reason 的比较或只比 discriminant）。**明确规定 `state_since_ms` 不作边沿信号。**

**边沿冷却（不是纯时间冷却）**：
- 记「上次通知时所处触发态」；会话一旦**离开**该触发态（进任何非触发态），立即清零该 `(session_id, kind)` 冷却。只压制**停在同一触发态内**的重复。
- prev 非触发态 → 触发态的**真边沿永远优先于冷却**——杜绝「你应答完、agent 又第二次等输入」被吞导致 agent 静默卡死。
- 冷却键用 `agent_session_id`（会话唯一 id）**而非裸 pane_id**（pane id 会复用/漂移）；`fresh_session` 重建时作废该 pane 的旧冷却/prev/generation 条目。

**stale 与 dead 拆成两个独立 kind**：各自冷却；允许严重度升级（Stale→Dead）绕过冷却发一次（长思考 vs 真崩溃严重度天差地别）。

**boot-grace 挡「重启即轰炸」（I4）**：daemon 因 tmux 退出 exit(0)+autospawn 频繁重启，通知器内存态重启即空。启动即以「快照加载到的既有会话状态」为边沿基线（首次观测同态不算跳变）；再加 `boot_grace_secs`（默认 30s）启动抑制期。这样不动快照就挡住轰炸。

**过期/错序防护（I5，实现修订）**：派发由 notifier **单线程独占 + FIFO mpsc 队列**串行化——事件按产出顺序到达、按顺序派发，天然不错序；同态重复由**边沿冷却**压制。M3 实测这两者已足够，故**不引入 generation 二次校验**（早期设计的 generation 字段在最终评审时移除：它需要 notifier 反向持有 Discipline、增加热路径锁竞争，收益仅是「偶发一条略陈旧的通知」）。若未来出现多消费者/多线程派发再重新引入。

**dead 去抖（I12）**：`reconcile_liveness` 在 tmux 部分缺失 pane 列表时会批量标 Dead。dead 通知需连续 K 轮（`dead_debounce_ticks` 默认 2）判死才发；下轮 pane 回来复活即撤销。

**hook 护栏（I6，验收清单硬项）**：`waiting_input` 由 Notification hook 驱动，流经 `server.rs::respond()`。`respond()` 锁内只收集 NotifyEvent 到 Vec，**出锁后非阻塞 mpsc send 入队立即返回**，一切通道 IO 归 notifier 线程。**`commands/hook.rs` 与 `client.rs` 永不得出现任何通知/配额代码，`tfa notify send` 只能 daemon 侧调用。**

## 7. 本地用量与 burn rate（吸收 C2/C3/I8/I10）

**consumed 计数器（C2）**：改 `read_update` 累加**每条**新 assistant 行的 delta 到 per-session `consumed`（累加 `output_tokens`，或 `input+output` 但**排除 `cache_read`** 避免重复计同一缓存上下文），按 `agent_session_id` 键。`tokens.total` 保留给上下文%显示，**绝不喂 burn rate**。codex `tokens_used`（per-thread 累计）统一到同一 consumed 口径。

**burn 聚合口径（C3）**：维护 provider 级 running `consumed` 累加器，**只加正 delta**（`max(0, new_cumulative - last_seen)`，按稳定 session_id/thread-id 追踪）。会话消失 / `prune` / SessionStart 重置 / 漂移重建 **贡献 0 而非负数**。环形缓冲存 `(ts, provider_consumed)`，口径 = 历史单调累加，**非「当前活跃会话之和」**。残余 Δ<0 防御性 clamp 到 0 并记 anomaly 日志。burn = 窗口内 Δ / Δt（`burn_rate_window_mins` 默认 60）。

**5h 滚动窗口建模（I8）**：Claude 订阅 5h 是滚动块、按小时取整锚定当前块首活动。`block_start = floor_to_hour(当前块首活动时间戳)`，`reset_at = block_start + 5h`；`now > reset_at` 时滚到下一个观测活动锚定的新块。锚定 provider 聚合的活动时间戳，**非单会话生日**。重置边界另起 burn 序列，绝不跨界 diff。

**诚实性（I10，防误读——正是用户最初担心的）**：`source=LocalEstimate` 时 `window_5h_percent`/`weekly_percent` **恒 None**（任何 UI 都渲染不出假%）。只暴露 `observed_tokens_this_window`（前缀 `≥`，limit unknown）+ `burn_rate`。本地值是系统性**低估**（claude `TAIL_CAP=262144` 首读丢历史、codex 只算每 cwd 最新 thread、daemon 停机/未 hook 会话不可见），UI 命名 "estimated usage — remaining unknown"，与未来真实 percent 放视觉不同字段永不混淆，始终内联 source 标签。

## 8. 通道实现（吸收 A2/A3/M1）

**macOS**：terminal-notifier「检测到就用」（可选依赖，注册为独立 app 拿独立通知权限，稳定可控 `-title/-subtitle/-sound`），否则 osascript `display notification` 兜底。**两个已知坑写进文档免责**：osascript 无权限时**静默失败**（exit 0 不可侦测）、通知挂 Script Editor 名下。**发送后不保证送达，失败绝不重试/告警。**

**tmux**：`display-message` 必带 `-t <pane_id>` 且复用 `paths::tmux_args()`（`-L <socket>`）。目标 session 无 attached client 时返回 `no clients` 类错误——**正常情况（用户没在看），daemon 必须当非致命吞掉**。bell 依赖用户 `monitor-bell on` 不可控，以 display-message 为主。

**HTTP 推送**：ureq POST，`format = bark | ntfy | generic-json`。硬超时 `timeout_ms`（默认 3000，上限约 10s）。`url` 空则即使 enabled 也静默跳过。**诚实声明（A2）**：iOS 后台/锁屏可达通知**必须经外网到 Apple APNs**，「自建 server」只减少第三方托管你的消息内容/信任面，**不消除对 APNs 的依赖，无纯 LAN 零外网 iOS 后台推送**。
- Bark：`POST /push` JSON，字段 `device_key`(必填)/`body`(必填)/`title`/`subtitle`/`sound`/`group`/`level`；bark-server 自建直连 APNs。
- ntfy：headers（`Title`/`Priority`/`Tags`）或 JSON POST；自建要 iOS 后台即时到达须 `upstream-base-url: https://ntfy.sh`。
- generic-json：tfa 定义 `{title, body, kind, session, ts}` schema + 可选 `headers`（webhook 鉴权）。

## 9. 快照兼容与安全（C4/I13）

- **QuotaState / burn 环形缓冲 / 冷却 map / generation / boot 基线一律不进序列化的 `StateStore`**，放独立 `Arc<Mutex<QuotaCache>>` 等易失态、每轮重算，从根绕开兼容问题。理由：漏一个 `#[serde(default)]` → 旧快照 missing field → `from_json` Err → `load_or_default()` 的 `.ok()` 吞成 None → `unwrap_or_default()` → **全部历史会话被无声清空**（比崩溃更难发现）。
- boot-grace（§6）已使「冷却态不持久化」无害——重启不轰炸靠启动播种基线 + grace，而非快照。
- **凭证安全**：M3 无 RealApiProvider，不读凭证，本节对 M3 为 N/A；留待未来「真实配额」里程碑（届时 token 用 newtype 包裹、Debug 输出 `***`、绝不进任何 serde/快照/日志——去风险结论 I13 已存档）。

## 10. 错误处理与生命周期

| 场景 | 行为 |
|---|---|
| 通道 IO 挂死（HTTP POST 到挂掉的 server / osascript 卡住） | notifier 独立线程 + 硬超时；绝不落在 scanner tick 或维护线程上（否则 daemon 变孤儿）。超时即放弃，不重试 |
| 通知发送失败 | 静默吞（TCC 静默失败/no clients/APNs 限流均属正常），不重试、不告警、不打断 daemon |
| daemon 重启 | 快照恢复会话 → 启动播种边沿基线 + boot-grace → 不轰炸 |
| tmux 部分缺失 pane | dead 去抖（连续 K 轮判死才发） |
| config 文件缺失/坏行 | 全用默认，不炸 |
| burn 序列 Δ<0 | clamp 0 + anomaly 日志 |
| codex 某轮读不到新 token | burn 采样标 freshness，不用陈旧值假装新鲜（M3） |

## 11. 测试策略

- **通知纪律**：纯逻辑单测——边沿快照 diff、边沿冷却（重点：应答后再等输入必须放行）、stale/dead 拆分与升级、boot-grace 抑制、dead 去抖、quiet_hours 窗口（含跨午夜 + 豁免集）。
- **通道 dispatch**：单测通道选择/格式化（bark/ntfy/generic-json payload 形状）、超时参数、url 空跳过。
- **burn/consumed**：单测 consumed 累加（排除 cache_read）、只加正 delta、prune/重置/漂移贡献 0、5h 窗口滚动、percent 恒 None。
- **快照兼容 e2e**：M1/M2 快照仍能 load（新态不进快照的钉子）。
- **e2e**：隔离 tmux（`-L`）+ 通知 sink（`TFA_NO_NOTIFY=1` 抑制真实副作用 / mock 通道）→ 断言净边沿只发一次、no-clients 非致命路径。
- **真机验收（用户，TCC 交互 agent 代劳不了）**：`tfa notify test` 走三通道 + 可能的系统授权弹窗；核对 setsid daemon 直接派发是否真弹得出、挂哪个 app 名下。

## 12. 任务分解预览（约 7 个，无 RealApiProvider）

① **config 模块**（config.toml 解析 + 默认 + `TFA_CONFIG_PATH`；schema 见 §13）。
② **consumed 计数器前置**（改 `read_update` 累加 delta 排除 cache_read + codex `tokens_used` 口径统一，按 session_id）。
③ **burn 采样器**（provider 聚合、只加正 delta、环形缓冲、5h 滚动窗口、重置另起序列；写 QuotaCache）。
④ **LocalEstimateProvider + QuotaState 接入 `tfa list`/`status --format json`**（percent 恒 None、observed ≥ 命名、source/freshness 内联）。
⑤ **notifier core + `tfa notify send`/`test`**（独立线程 + mpsc 队列 + 硬超时 + ureq + 三通道纯派发）。**完成即用户真机 `tfa notify test` 独立验收三通道**（CLAUDE.md 逐个验收纪律 + TCC 交互）再进 ⑥。
⑥ **通知纪律 + 四触发接线**（tick 边界快照 diff、边沿冷却、stale/dead 拆分、boot-grace、dead 去抖、quiet_hours 豁免；接 waiting_input/done/stale/dead；hook 护栏）。
⑦ **e2e + 文档 + 真装验收**（含 no-clients 非致命、快照兼容 e2e；Bark/ntfy 配置指引 + APNs 需外网诚实说明）。

## 13. config schema（`~/.config/tfa/config.toml`，缺失全默认）

```toml
[notify]
enabled = true
# 免打扰时段（可选，不填=不启用）。静默 waiting_input/done/stale；dead 豁免（真崩溃仍穿透）。
# quota_alert 未来启用后同样豁免（用户决策：豁免配额告警+异常死亡）。每类可在下方按需覆盖。
quiet_hours = { start = "23:00", end = "08:00" }
# quiet_hours_exempt = ["dead"]   # 默认豁免集；未来加 "quota_alert"

[notify.channels.tmux]   # 零成本，默认开
enabled = true
[notify.channels.macos]  # 默认开
enabled = true
[notify.channels.http]   # 默认关；url 空即使 enabled 也静默跳过
enabled = false
url = ""
format = "bark"          # bark | ntfy | generic-json
timeout_ms = 3000        # 硬超时上限约 10s（C1）
headers = {}             # ntfy 访问控制 / generic webhook 鉴权
# 可选：channel 级 triggers 覆盖（缺省继承全局）
# triggers = ["dead"]

[notify.triggers]        # M3 四开关（quota_alert 推后，本里程碑无此项）
waiting_input = true     # 默认开
done          = false    # 默认关（易吵）
stale         = false    # 默认关
dead          = false    # 默认关

[notify.discipline]
cooldown_secs       = 30 # per-(session_id,kind) 边沿冷却
dead_debounce_ticks = 2  # 连续 K 轮判死才发（I12）
boot_grace_secs     = 30 # 启动抑制期（I4）

[quota]                  # M3 仅本地推算显示，不主动告警
burn_rate_window_mins = 60
# 以下为未来「真实配额」里程碑预留，M3 未启用：
# poll_interval_secs / real_max_age_secs / real_fail_debounce /
# window_alert_threshold_pct / weekly_alert_threshold_pct / alert_disarm_pct
```

**config vs env var 分工**：面向用户偏好只活在 config.toml，不为其重复开 env var；`TFA_*` 继续只服务「测试隔离/路径覆盖」逃生舱（新增 `TFA_CONFIG_PATH` 指 fixture、`TFA_NO_NOTIFY` 抑制真实通知副作用）。这类测试逃生舱可无条件赢 config（不同目的，非同一开关两来源）。

## 14. 依赖选型

- HTTP：**ureq（rustls feature）**——纯阻塞、不拉 tokio、自带 `timeout_connect/timeout_read`（正好满足硬超时），一个依赖覆盖 Bark/ntfy 推送。**明确禁 reqwest**（编入 tokio 异步核心，违反「无 async runtime」+「节制依赖」）。
- toml 解析：轻量 `toml` crate。
- 通知：terminal-notifier 可选「检测到就用」，osascript 无依赖兜底；不引 Notifier（未上 brew）。
- codex 沿用现有 rusqlite 只读。

## 15. 里程碑与路线展望

| 里程碑 | 内容 | 状态 |
|---|---|---|
| M1 | daemon + hook + 状态机 + 状态栏 | ✅ 已上线 |
| M2 | scanner 兜底 + 资源指标 | ✅ 已上线 |
| **M3** | **本地用量/burn rate + 主动通知（macOS/tmux/手机推送）** | **本 spec** |
| 未来「真实配额」 | RealApiProvider（Keychain OAuth + `GET api.anthropic.com/api/oauth/usage`，去风险已查明端点/字段/头/凭证读法，见下）+ `quota_alert` 点亮 + sticky/TTL 降级 + 凭证 newtype 脱敏 | 已去风险，待排期 |
| M4 | **只读同步仪表盘**：LAN HTTP/web 服务把全部 agent 状态吐到手机+桌面浏览器**实时看**（用户想要的「同步看」）。subscribe 流在此引入 | 展望 |
| M5 | **远程操作 + 语音**：`tmux send-keys` 执行器 + 设备配对/token 鉴权 + 手机 app 原生语音输入。观测→接管的使命扩张，**独立 brainstorm→spec 周期** | 展望 |

**M5 安全红线（提前记档）**：一个能往你 agent 注入按键的网络端点 = 严重攻击面（可替你批准危险权限、跑命令）。**绝不能是「同一局域网就行」，必须设备配对 + token 鉴权 + TLS。** 「看」（M4，只读，低危）与「操作」（M5，可写，高危）分开对待。

**未来「真实配额」里程碑已查明的实现事实（去风险存档）**：
- 凭证：macOS Keychain service 名 `"Claude Code-credentials"`，`security find-generic-password -a "$USER" -s "Claude Code-credentials" -w` → JSON 取 `claudeAiOauth.accessToken`；非 macOS fallback `~/.claude/.credentials.json` 或环境变量 `CLAUDE_CODE_OAUTH_TOKEN`。
- 端点：`GET https://api.anthropic.com/api/oauth/usage`，必带 `Authorization: Bearer <token>`、`anthropic-beta: oauth-2025-04-20`、`User-Agent: claude-code/<version>`（**缺 UA 秒 429**）、`Content-Type: application/json`。
- 响应：`five_hour:{utilization, resets_at}`、`seven_day:{utilization, resets_at}`（utilization 0-100，resets_at ISO8601 UTC）。
- **该端点未公开、非官方、可能随时变**——故降级到 Local 是硬需求；实现前需 clone `ryoppippi/ccusage` + `ohugonnot/claude-code-statusline` 读源码 + 用户授权手动 curl 核对响应体。

## 16. 风险与开放问题

| 项 | 风险/状态 | 缓解 |
|---|---|---|
| 通知送达 | TCC 静默失败 / APNs 限流（约 2-3 次/小时）/ no clients | 发送后不保证送达；失败不重试不告警；真机 `tfa notify test` 验收 |
| iOS 手机推送 | 后台送达必经 APNs 需外网，非纯 LAN | 文档诚实写明；纯 LAN 实时看板留 M4 |
| terminal-notifier | 已停维护（2021，废弃 API），仍可用 | 可选依赖「检测到就用」；osascript 兜底 |
| codex burn rate 新鲜度 | codex sqlite schema 为 M2 现场探明 | 采样标 freshness，不假装新鲜 |
| daemon 频繁重启 | tmux 退出即 exit(0)+autospawn | boot 基线播种 + grace 挡轰炸 |
| generic-json 接收端形状 | 若用户自建 webhook，schema 未定 | config `headers` + 默认 `{title,body,kind,session,ts}`，用户可在验收时提形状 |
```