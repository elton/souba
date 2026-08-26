# Cloudflare 免费版额度实测与约束

**日期：** 2026-08-26　**来源：** developers.cloudflare.com 当日检索 + 本账户 API 实查。

本项目明确只用 **free 订阅**。免费版有几条限制会直接决定架构，而不是「先做了再说、超了再优化」。

## 一、会改变架构的三条硬约束

### 1. **所有**计算入口都是 10 ms CPU —— Worker、Durable Object、Cron 一视同仁

这是最容易误判的一条。Workers 免费版的**墙钟时间不限**（HTTP 触发的 Worker 只要客户端连着就能一直跑），
但 **CPU 时间每次调用上限 10 ms**。Cloudflare 自己的文档说「解析大 JSON 负载」典型消耗 10–20 ms。

**而且这个限制没有例外。** DO 限制 FAQ 原文：「Durable Objects are Worker scripts, and have the same
per invocation CPU limits as any Workers do」。Cron Trigger 同样是 10 ms（付费版才是 30 s）。

**推论一：指标计算绝不能放在 Worker 上。**
`EMA576` 要跑 1800 根 K 线，MACD/KDJ 再叠上去，10 ms 根本不够。
→ **指标在 Rust TUI 客户端本地算。** 这同时也让客户端离线可用。

**推论二：冷启动的历史回补也不能放在 Worker 上。**
一次拉 1800 根 K 线，光 `JSON.parse` 就可能吃掉大半个 CPU 预算。
→ **冷启动回补由 TUI 完成**（本地无 CPU 限制），拉到后分批 POST 给 Worker 落库。
→ **Cron 只做每日增量追加** —— 收盘后每标的每周期新增 1 根，payload 极小，10 ms 绰绰有余。

这条修正很关键：原设计里「Cron 负责补历史」是错的，只有「Cron 负责每日追加」才成立。

### 2. Workers KV 每天只能写 **1000 次**

平均 86 秒才能写一次，且 KV 是最终一致，全球传播可能超过 60 秒。
**KV 不能用来缓存行情。** 需要写入的状态放 D1 或 Durable Objects。

### 3. Durable Objects 一个常驻对象吃掉 **85% 日额度**

免费版 DO 可用（仅 SQLite 后端），但 duration 额度是 13,000 GB-s/天。
按计费的 128 MB 分配折算 ≈ **101,500 对象秒/天**，而一天只有 86,400 秒。

**一个 24 小时常驻不休眠的 DO 就消耗掉约 85% 的全天额度。**
且文档规定一条对外 WebSocket 最多只能让对象保活 15 分钟。

**推论：不能用 DO 常驻连接行情供应商。** DO 只在需要时唤醒（hibernation 免费）。

## 二、额度明细

| 组件 | 免费额度 | 对本项目的影响 |
|---|---|---|
| Workers 请求 | 100,000/天（全账户） | 充裕 |
| Workers CPU | **10 ms/次调用** | **决定性约束，见上** |
| Workers 子请求 | 50/次调用 | 一次批量拉多只股票要注意 |
| Workers 内存 | 128 MB/isolate | 够用 |
| **Cron Triggers** | **全账户 5 个**，最小间隔 1 分钟 | 本账户已用 1 个（`eltonzheng-me` 的 `*/5 * * * *`），**souba 可用 4 个** |
| D1 行写入 | 100,000 行/天 | 日线够；多标的 5 分钟线会紧张 |
| D1 行读取 | 5,000,000 行/天 | 充裕 |
| D1 单库大小 | 500 MB（账户共 5 GB，10 个库） | 需要估算多市场多周期的存储量 |
| D1 每次调用查询数 | **50**（与子请求上限绑定） | 批量写入要合并语句 |
| KV 写入 | **1,000/天** | 不可用于行情 |
| KV 读取 | 100,000/天 | 可用于低频配置 |
| Queues | 10,000 操作/天 ≈ **3,333 条消息/天**，保留 24 小时不可配 | 够用但不宽裕 |
| R2 | Class A 100 万/月、Class B 1000 万/月（需单独开通订阅） | 适合存历史归档，不适合逐笔状态 |
| Workers AI | **10,000 neurons/天** | 见下 |
| WebSocket | ✅ 免费版支持，**无时长/空闲限制**，只要客户端连着 | TUI 长连接可行 |

### WebSocket 是免费版少数「不打折」的能力

普通 Worker 的 WebSocket 最省额度：升级握手算 **1 次请求**，之后**消息不计费、时长不计费**。

DO 的情况需要修正一处：DO 计费脚注写明「There is no charge for outgoing WebSocket messages」——
**出站消息不计费**，只有入站消息计入 DO 的 100,000/天。所以 DO 推送并不像初版写的那么贵。

**但推论不变：TUI 的长连接走普通 Worker WebSocket。** 因为 DO 真正的问题是 10 ms CPU 和
duration 额度，不是消息计费。

### Workers AI 的 10,000 neurons 有多少

已实测 `@cf/qwen/qwen3.8-27b`：40,909 neurons/M 输入 token，290,909 neurons/M 输出 token。

- 全部用于输出 ≈ 34,000 output tokens/天
- 典型场景（500 输入 / 400 输出，关闭思考）≈ **73 次/天**

**够做兜底，不够做主力。** 主力 LLM 走 DashScope 的 `qwen3.7-flash`。

**两个模型都默认开思考模式，且 reasoning token 按输出价计费。** 实测 qwen3.7-flash
一次简单问答 840 个输出 token 里 **784 个是 reasoning**（93%）。必须在调用层关掉：

- Workers AI：`chat_template_kwargs: {"enable_thinking": false}`
  （顶层的 `enable_thinking` 参数会被**静默忽略**，实测无效）
- DashScope：`enable_thinking: false`

## 三、由此推导出的架构分工

```
┌─ souba TUI (Rust) ─────────────────┐
│  · 实时行情：直连腾讯/新浪           │  ← 不经 Worker，零额度消耗，少一跳
│  · 指标计算：本地算（10ms 限制的解）  │
│  · K线渲染、自选股本地缓存           │
└──────────────┬─────────────────────┘
               │ HTTPS / WebSocket（免费版无时长限制）
┌──────────────▼─────────────────────┐
│  Cloudflare Worker（薄层，不做计算） │
│   · D1：历史 K 线长期落库累积        │
│   · Cron（4 个可用）：每日收盘后补历史│
│   · LLM 代理：qwen3.7-flash / CF AI  │
└────────────────────────────────────┘
```

**核心思想：Worker 是存储层和代理层，不是计算层。**
即使 Worker 挂掉或额度耗尽，TUI 依然能直连数据源看盘 —— 这是免费版下正确的降级姿态。

## 四、尚未验证的最大风险

**本文所有数据源测试都是从一台日本住宅 IP 打的，没有一次来自真正的 Cloudflare Worker 出口。**

Worker 用的是共享数据中心 IP，行为可能完全不同：

- Yahoo 已被证实对数据中心 IP 极度敌视（第一个请求就 429）
- 腾讯/新浪对海外数据中心 IP 段是否有额外限制，未知
- 东财已确认封禁，Worker 只会更早触发

**这个风险由架构本身化解**：既然实时行情由 TUI 直连（住宅 IP），Worker 只做落库和 LLM 代理，
那么数据源对 Worker IP 的态度就只影响每日增量追加这一条路径。真到不行，追加也可以由 TUI 完成，
Worker 退化为纯存储。

**但仍需在实现第一步就实测验证**，不要等到设计全部落地才发现 Worker 拉不到数据。
