# tfa 真实配额（Real Quota）设计文档

> 里程碑：真实订阅配额（M3 spec §15 存档的「未来真实配额」，正式排期）。
> 状态：设计定稿（brainstorm 2026-07-13，风险决策：做、**默认关闭 opt-in**）。
> 前置事实：2026-07-13 现场核实（clone claude-code-statusline 读源码 + 本机凭证位置确认）。

## 1. 背景与目标

M3 交付的配额是**本地估算**：`observed_tokens_this_window`（`≥` 下界）+ burn rate，
`window_5h_percent` 恒 `None`（诚实：无真实 limit 不编造百分比）。本里程碑把它升级为
**真实订阅配额**：调用 Claude Code 官方客户端同款（但无文档、非官方）的 usage 接口，
拿到 5 小时窗 / 7 天窗 / 7 天 Sonnet 窗的真实 utilization 百分比与重置时间，
并点亮 M3 预留的 `quota_alert` 主动通知。

**成功标准**：
- config 开启后，TUI/`tfa list` 显示真实百分比与重置时间，来源标注 `real_api`。
- 5h 窗越过阈值时收到一次主动通知（迟滞去重，不轰炸）。
- config 不开启（默认）时，**tfa 的行为与本里程碑之前逐字节一致**：零 API 调用、零 Keychain 访问。
- 接口失败/下线时自动降级回本地估算，UI 永不显示假数据。

## 2. 风险决策存档（用户 2026-07-13 拍板）

- **封号风险**：低但非零。接口即官方客户端所调（读自己账户的用量数字，只读不写）；
  社区工具（claude-code-statusline 等）以 5 分钟间隔公开使用已久、无已知封号案例；
  条款层面非官方客户端调用无人能打包票 → **对冲 = 默认关闭 + 温和调用模式**（见 §6）。
- **读取方式与封号无关**：Anthropic 只见 API 调用，不见本机取 token 的方式。
  读取方式只影响本机凭证卫生 → 选 Keychain 自动读（token 仅内存、自动跟随轮换），
  不做手填 config（明文落盘 + 过期失效）。
- 决策点①：状态栏段**默认不加**百分比（config 可开）。决策点②：告警阈值默认 5h=85 / 7d=90。

## 3. 关键事实（2026-07-13 现场核实）

1. **端点**：`GET https://api.anthropic.com/api/oauth/usage`。
   头：`Authorization: Bearer <accessToken>`、`anthropic-beta: oauth-2025-04-20`、
   `Content-Type: application/json`。**User-Agent**：statusline 用 curl 默认 UA 正常工作
   （M3 记录的「缺 UA 秒 429」已过时/过度谨慎）；tfa 发诚实 UA `tfa/<version>`。
2. **响应**（statusline CLAUDE.md 记录 + 待任务 1 真机 curl 钉死）：
   ```json
   {
     "five_hour":        { "utilization": 18.0, "resets_at": "2026-03-27T10:00:00+00:00" },
     "seven_day":        { "utilization": 17.0, "resets_at": "2026-04-02T13:00:00+00:00" },
     "seven_day_sonnet": { "utilization": 10.0, "resets_at": "2026-04-02T13:00:00+00:00" }
   }
   ```
   utilization 为 0-100 **浮点**；`seven_day_sonnet` 是 M3 记录之后新增的窗口，可能缺席
   （`Option` 处理）。未来还可能加窗口 → 反序列化必须容忍未知字段。
3. **凭证**：本机（用户 Mac）**只有 Keychain**（`~/.claude/.credentials.json` 不存在，已核实）。
   Keychain 条目 service=`"Claude Code-credentials"` 存在（已核实元数据）。读法：
   `security find-generic-password -a "$USER" -s "Claude Code-credentials" -w` → JSON 取
   `claudeAiOauth.accessToken`。首次读取 macOS 会弹授权框（用户点允许）。
   非 macOS fallback 链：`~/.claude/.credentials.json` → env `CLAUDE_CODE_OAUTH_TOKEN`。
4. **依赖**：HTTP 走既有 `ureq`（rustls+json，M3 引入），**零新增依赖**。
   ISO8601（固定 `+00:00`/`Z` 后缀形态）→ epoch ms 用纯函数手解（days-from-civil 算法，
   单测钉死），不引日期库。
5. 社区参照的节奏纪律：statusline 默认 300s 刷新并明确警告「不要设 0（有限流）」。

## 4. 总体架构

方案 A（brainstorm 选定）：**daemon 内置 fetcher 线程**，与 M3 notifier 线程同构。

```
daemon ──spawn(仅当 config.quota.real)──► quota_real 线程
  loop {
    token = Keychain(缓存，401 时重读一次)
    resp  = GET oauth/usage (5s 超时，单飞)
    ok  → RealQuota{三窗口, fetched_at} 写入 Arc<Mutex<RealQuotaCell>>；退避复位
    err → 指数退避 (10m→20m→40m→…→2h 封顶)；429 尊重 Retry-After
    评估 quota_alert 阈值（迟滞）→ 命中则经既有 mpsc 发 NotifyEvent
    sleep(refresh_secs 或退避值)
  }
快照组装（既有 tick 路径）：QuotaCache::refresh 产出 LocalEstimate 后，
若 RealQuotaCell 数据年龄 < TTL(30min) → 用真实值覆盖 percent/reset 字段并标 source=RealApi。
```

- **token 只存在于 daemon 此线程与 Keychain 之间**：newtype `AccessToken`（手写 Debug/Display
  输出 `***`，**不实现 Serialize**），绝不进快照/日志/`tfa list`/任何 IO。
- 客户端（tui/status/list）零改动数据通道：真实值随既有 `Response::Snapshot.quota` 分发。
- 无 async；线程 + sleep + mpsc，延续全项目不变量。

## 5. 数据模型（快照加法）

`QuotaState` 增量（全部 `#[serde(default)]`，老快照兼容测试照钉）：

- `weekly_sonnet_percent: Option<u8>`（新）
- `weekly_reset_at_ms: Option<u64>`（新；现有 `reset_at_ms` 语义不变=5h 窗重置）
- 复用既有：`window_5h_percent`/`weekly_percent`（真实值时 `Some(round(utilization))`）、
  `reset_at_ms`（真实值时=API `resets_at`，且 `reset_estimated=false`）、`freshness_ms`
  （真实值时=fetch 时刻）。
- `QuotaSource` 增变体 `RealApi`（serde `"real_api"`）。**向前兼容注记**：旧二进制读含新变体
  的快照会反序列化失败——可接受（client 与 daemon 同一二进制、autospawn 用同一 exe，
  不存在长期新旧混跑）；快照「只做加法」不变量指旧快照必须能被新代码加载（照钉）。
- LocalEstimate 与 RealApi 的**合并语义**：RealApi 覆盖 percent/reset 字段，
  `observed_tokens_this_window`/`burn_rate_per_min` 始终来自本地采样（两者互补展示）。

## 6. 请求纪律（封号对冲的落地，逐条 spec 级约束）

1. 默认 `real = false`：不开启则 **fetcher 线程根本不 spawn**，零 Keychain 读、零网络调用
   （有测试钉：默认 config 下 daemon 不产生任何新子进程/连接路径）。
2. `refresh_secs` 默认 600，**下限钳制 300**（配置更小值按 300 处理）。
3. 单飞：线程内串行，永无并发请求。超时 5s。
4. **任何失败一次即退避**：10m → 20m → 40m → 80m → 2h 封顶；成功一次复位。
   429 带 `Retry-After` 时取 max(退避值, Retry-After)。401/403 → 重读一次 Keychain
   （token 轮换自愈），仍失败则按退避走，**绝不循环重试**。
5. 响应解析失败（形状变了）视为失败并退避——接口变更不会造成请求风暴。

## 7. 降级语义（诚实性延续 M3 §诚实性）

- 真实值**粘滞 TTL = 30min**：拉取失败期间，上一次真实值继续展示，`freshness_ms` 如实
  标注年龄（UI 显示「N 分钟前」）。
- 年龄超 TTL → 丢弃真实值，回落 LocalEstimate（percent 恒 None、`≥` 前缀，与现状一致）。
- 任何 UI 永远内联 source 标签（`real_api` / `local_estimate`），两种形态视觉可区分，
  绝不把估算渲染成真实百分比。

## 8. 展示

- **TUI header**（开启且新鲜时）：`claude 5h 62%·7d 31%`（替换该位置的 burn 概览；
  burn 移入详情栏）。Sonnet 窗仅在详情栏展示（避免 header 过载）。
- **TUI 详情栏**：三窗口各自 percent + 重置时间（本地时区渲染）+ 数据年龄 + source。
- **`tfa list`**：新字段自然流出（§5）。
- **状态栏**：默认不变。`[quota] status_bar_percent = true` 时 `tfa status --format tmux`
  追加 5h 百分比 chip（如 `⚡1 ⏸2 62%`）；status 客户端为此每次调用读一次 config（5s 间隔
  的子进程，成本可忽略）。
- 中英双语：新 UI 串全部进 i18n Texts（en/zh 各一份）。

## 9. quota_alert 主动通知（点亮 M3 预留触发器）

- 阈值（config 可改）：`alert_5h = 85`、`alert_7d = 90`；设 0 = 关闭该窗告警。
- **迟滞**：5h 窗 ≥85 触发一次后武装解除，回落 <80 重新武装（窗口每 5h 自然重置也会回落
  重武装）；7d 同理（≥90 触发，<85 重武装）。杜绝在阈值附近来回横跳轰炸。
- 通知体走 M3 既有通道/quiet_hours/去重管线（`NotifyEvent kind="quota_alert"`），
  quiet_hours **不豁免**（默认豁免集仍仅 `["dead"]`）。
- 文案示例：`Claude 5h 窗口已用 87%，14:00 重置`（en/zh）。

## 10. config（`[quota]` 段扩展，缺省全等于现状）

```toml
[quota]
real = false               # 总开关：默认关，关=零 API/零 Keychain
refresh_secs = 600         # 轮询间隔，下限 300
status_bar_percent = false # 状态栏追加 5h% chip
alert_5h = 85              # 5h 窗告警阈值，0=关
alert_7d = 90              # 7d 窗告警阈值，0=关
```

M5-发布轮已落地的「按 section 降级解析」自然覆盖新字段（typo 只废 quota 段）。

## 11. 测试策略

- **纯逻辑单测**：ISO8601→ms 解析（含 Z/+00:00/闰年边界）；退避阶梯状态机；迟滞武装状态机；
  TTL 粘滞→回落；LocalEstimate/RealApi 合并；AccessToken Debug 打码。
- **mock HTTP 集成**：std TcpListener 手写极小 blocking 响应器（零新依赖），覆盖
  200/401（触发 Keychain 重读）/429+Retry-After/超时/畸形 JSON → 断言退避与降级行为。
- **默认关闭钉死**：默认 config 下 fetcher 不 spawn；e2e（TFA_NO_NOTIFY sink）无 quota_alert
  事件；现有 184 测试零改动通过。
- **凭证脱敏钉死**：快照 JSON / `tfa list` 输出 / Debug 格式化中 grep 不到 token 值。
- **真机验收（人工）**：任务 1 的单次 curl fixture；开启后 TUI 真实 %；阈值调低诱发一次
  quota_alert 真通知；关闭开关行为回退。

## 12. 实现前验证（plan 任务 1，需用户届时授权）

经用户明示授权后执行一次：读 Keychain token → 单次 curl 真实接口 → 响应体
（脱敏检查：确认无账户标识等敏感字段后）存为测试 fixture，钉死解析器。
**未获授权不碰 Keychain**。若响应形状与 §3.2 不符，回到设计修订字段映射再实施。

## 13. 任务分解预览（约 5-6 个）

1. 真机验证 curl + fixture（用户授权门）+ ISO8601 纯函数
2. config `[quota]` 扩展 + AccessToken newtype + Keychain/凭证链读取（含脱敏测试）
3. fetcher 线程：请求纪律全套（单飞/退避/429/401 重读）+ mock HTTP 测试
4. 快照合并：QuotaState 加字段 + RealApi source + TTL 粘滞降级 + 兼容测试
5. 展示：TUI header/详情 + status_bar_percent + i18n 串 + `tfa list`
6. quota_alert：迟滞触发 + notify 管线接入 + e2e（sink 断言）+ 文档（README 双语 + spec 注记）

## 14. 已知限制与风险

- 接口非官方可能随时变/关 → §6.5 + §7 保证安全降级，风险只在功能失效不在行为异常。
- utilization 是账户级（不分 provider 内会话）；codex 无此接口，本里程碑仅 Claude。
- 封号残余风险：温和模式下 ≈ statusline 用户暴露水平；用户已知情拍板（§2）。
- Keychain 授权框只在 daemon 首次读时弹一次；若用户点拒绝 → 读取失败按退避处理，
  UI 保持本地估算（不反复骚扰弹框：退避机制天然限频）。
