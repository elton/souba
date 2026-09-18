# souba（相場）

> 一个跑在终端里的多市场股票行情与策略终端。

`相場（そうば）` 是日语「行情、市价」。这个项目就是一个行情终端 —— 打开终端就能看到自选股实时跳动，跑策略选股，问 AI 要建议。

**状态：A 股闭环已跑通** —— 看盘、K 线与指标、板块扫描、Vegas 策略、AI 解读、D1 同步与每日追加，
A 股全链路可用；港股 / 美股 / 日股目前只有实时报价与 K 线，策略层等历史源接入后再铺。

```sh
cargo run --release                 # 打开 TUI（自选股列表）
souba 600519                        # 直接打开个股详情
souba scan                          # 不带 UI 跑一次当日板块扫描（可挂 launchd）
souba sync                          # 手动与 Worker 同步一次
souba settings                      # 列出全部策略与扫描参数；souba set / get 修改与查看
```

设计方案见 [`docs/decisions/2026-08-26-souba-设计.md`](docs/decisions/2026-08-26-souba-设计.md)，
本块的 spec 见 [`docs/specs/2026-09-18-A股闭环.md`](docs/specs/2026-09-18-A股闭环.md)（开头一页是四市场共用的架构决定），
支撑它们的实测调研见 [`docs/research/`](docs/research/)。

| 阶段 | 内容 | 状态 |
|---|---|---|
| 0 | Cloudflare Worker 出口可达性验证 | ✅ 全部打通，无封禁 |
| 1 | 四市场报价 + 自选股 + 自适应 TUI | ✅ |
| 2 | 指标引擎 + K 线渲染 + 十字光标 + 位图/盲文两种后端 | ✅ |
| 3 | 港股 / 美股 / 日股的板块池、历史源与降级链 | 未开始（架构已定，见 spec 一页纸） |
| 4 | 历史落库 + 板块扫描 + 策略引擎 + Vegas + D1 同步 | ✅ A 股 |
| 5 | AI 解读 | ✅ 单只与整组 |

## 怎么用

**自选股列表**：`↑↓`/`jk` 选、`Enter` 详情、`s` 机会面板、`q` 退出。

**详情屏**：`←→` 滚动、`=`/`-` 或滚轮缩放、鼠标悬停读数、`Tab` 切周期、`i` 切指标（MACD / KDJ）、
`↑↓` 切标的、`?` 让 AI 解读这只的策略信号、`Esc` 返回。底部一行是 Vegas 信号：主判定加五个维度
（趋势 / 共振 / 位置 / 确认 / 预备）。日线不足 1330 根时显示「数据不足」，不会拿一条不可信的慢隧道装作有结论。
A 股日线第一次打开会从新浪回补全量原始日线（约 6000 根）与前复权因子，之后从本地库读，读时复权。

**机会面板**（`s`）：每天第一次打开 TUI 会在后台自动扫一次；也可以 `souba scan` 盘后先算好。
上半是按热度排序的板块（行业 + 概念），下半是光标板块的 top 5。`Tab` 在两个列表间切、`Enter` 看详情、
`a` 加自选、`r` 重扫、`?` 让 AI 把当天所有 top 排序并解释。
「回补中」是历史还没补完，「数据不足」是补完了但根数不够，「预备」是 Long 不够时用「快要突破」的 Watch 补的位。

**AI**：直连 DashScope 的 OpenAI 兼容端点，思考模式关闭；同一组信号同一天只问一次（`R` 强制重问）。
输出永远以「机器生成的策略分析，不构成投资建议」开头。

**参数**：`souba set vegas.fast 144,169`、`souba set scan.k 20`…… 全部键见 `souba settings`，非法值会被拒绝并说明原因。

## 扫描是怎么算的

1. 拉新浪的行业板块与概念板块列表（一次各一个请求），落当日快照。
2. 热度 = `w_turnover × (今日成交额 / 前 N 日均值) + w_change × N 日累计涨幅`，取前 `scan.sectors` 个板块。
   快照不足 N 日按实际天数，首日退化为当日涨幅。
3. 每个热门板块取当日涨幅前 `scan.k` 只做候选；跨板块去重；缺历史的排队回补（新浪 7 秒一只，首日约 20 分钟，断点续补）。
4. 对每只候选跑 Vegas，板块内按「趋势确立的新鲜度」排序取 `scan.top` 只：刚站上快隧道的排最前。
5. 结果落 `scan_results`，机会面板只读它，同日重跑覆盖。

## `.env`

`.env`（gitignored，mode 600）按「进程环境 → 当前目录 → 用户数据目录」三级查找：

| 键 | 用途 |
|---|---|
| `LLM_BASE_URL` / `LLM_API_KEY` / `LLM_MODEL` | AI 解读；`souba set ai.model` 可覆盖模型 |
| `SOUBA_SYNC_URL` / `SOUBA_SYNC_KEY` | Worker 同步；key 与 Worker 侧 secret 同值 |
| `CLOUDFLARE_ACCOUNT_ID` / `CLOUDFLARE_API_TOKEN` | 只给 wrangler 用；环境里同名的 token 属于另一个账户 |

---

## 想做成什么

| # | 能力 | 说明 |
|---|---|---|
| 1 | **多市场行情** | A股、港股、美股、日股，统一的代码与报价模型 |
| 2 | **多周期 K 线** | 日 / 周 / 月 / 60分 / 15分 / 5分 |
| 3 | **可配置指标** | MACD、KDJ、RSI、BOLL、EMA…… 参数全部可调 |
| 4 | **可插拔策略** | 首个实现是 Vegas 隧道战法（见下） |
| 5 | **AI 选股** | 默认 `qwen3.7-flash`，模型可换；用策略产出的信号让大模型给推荐 |
| 6 | **自选股** | 代码快速搜索、一键加自选、实时刷新 |
| 7 | **终端 UI** | 自动刷新的 TUI，不用离开终端 |

## 首个策略：Vegas 隧道

三组 EMA 加三条规则，全部可代码化：

```
EMA12                 过滤器
EMA144 / EMA169       快隧道 ← 价格在其上方才考虑做多
EMA576 / EMA676       慢隧道（= 快隧道 × 4）← 决定持仓与离场
```

1. **隧道上方只做多，下方只看不做** —— 解决逆势抄底
2. **`EMA12` 必须同步站上隧道才算真突破** —— 解决真假突破
3. **慢隧道不走平、不拐头就持仓不动；掉头向下全部离场** —— 解决拿不住单

隧道是**区间**不是单线：单根 K 线短暂刺破不算有效突破，这是它比普通均线抗插针的原因。

完整规则与推导见 [`docs/research/vegas-tunnel.md`](docs/research/vegas-tunnel.md)。

## 已验证的关键约束

两轮实测（2026-08-26、2026-09-18）得出的会直接决定架构的事实，细节见 [`docs/research/2026-08-26-datasource-probe.md`](docs/research/2026-08-26-datasource-probe.md)：

1. **腾讯 `qt.gtimg.cn` 一个源覆盖全部四个市场的实时报价**，免费、无需注册。但返回 **GBK** 编码。
2. **港股的免费实时藏在前缀里** —— `r_hk00700`（腾讯）/ `rt_hk00700`（新浪）只慢 1 秒，而同一台主机上的 `hk00700` 慢 15–22 分钟。日股没有这个实时孪生。
3. **各市场延迟不同且会漂移** —— 必须用「响应时间戳 vs 市场时钟」在运行时实算并逐条标注，不能写死常量。
4. **A股 深度历史已解决，但是原始价** —— 新浪 `CN_MarketData.getKLineData` 给全量日线（茅台 5990 根，到 2001 年），
   `EMA576` 的种子残留只有 9e-10。它**不复权**，因子在 `realstock/company/{code}/qfq.js`；本地存原始价 + 因子表、读时复权。
   美股可用 Alpaca（免费档日线到 2016、历史默认 SIP 全量），日股可爬 Yahoo 日本站的 history 页（到 2010）；港股仍只有 640 根，靠 Cron 逐日累积。
5. **所有免费源都会封 IP** —— 数据层必须内建节流、缓存和退避。

## Worker（同步与 Cron）

`worker/` 是存储与代理层，不做任何指标计算 —— 免费版每次调用只有 10 ms CPU。
D1 库 `souba`，绑定名 `DB`；所有接口都要请求头 `X-Souba-Key`，值在 `.env` 的
`SOUBA_SYNC_KEY`，Worker 侧是同名 secret。

```sh
cd worker && pnpm install
pnpm test                                   # vitest-pool-workers，跑本地 D1

# 下面每一条都必须先 source 本项目的 .env —— 环境里的 CLOUDFLARE_API_TOKEN 属于另一个账户
set -a && . ../.env && set +a
pnpm exec wrangler whoami                   # 账户必须是 10100291fe85c71daed5efb6a3ff7795
pnpm exec wrangler d1 migrations apply souba --remote
pnpm exec wrangler secret put SOUBA_SYNC_KEY
pnpm exec wrangler deploy
```

| 接口 | 作用 |
|---|---|
| `POST /sync/bars` | 批量 upsert，单次上限 2000 根，超了返回 413 |
| `GET /sync/bars?since=<ts>` | 拉 `ts` 严格大于 `since` 的 bar，单页 5000，用返回的 `next` 续拉；`symbol=` 可按标的全量拉 |
| `GET\|PUT /sync/watchlist`、`/sync/settings` | 整表交换，服务端只在传入 `updated_at` 不早于库里时覆盖，不删行 |
| `POST /sync/sectors` | `sectors` / `sector_daily` / `sector_members` / `scan_results` / `adj_factors` / `backfill_state` 按主键 upsert |
| `GET /sync/sectors?since=<ts>&since_date=<日期>` | 上述六表的增量；`sectors`/`backfill_state` 按 `updated_at`，其余按业务日期 |
| `/probe` | 阶段 0 的出口可达性探测，改 `PROBE_TARGETS` 即可换目标 |

Cron `0 * * * *`：按各市场时钟挑出「已收盘且今日未追加」的市场，用腾讯批量报价
（100 只一批）给库里每只标的补上当天那根原始日线。目前只实现 A 股分支，追加点定在 16:00 CST
（15:00 整点是收盘集合竞价落槌的瞬间，那一刻的报价可能还是竞价前的价）。
2026-09-18 实测：单只单批 CPU 5.5 ms、wall 1.6 s；满 100 只一批时要再看一次 CPU，接近 10 ms 就把每批缩到 50。

本地 SQLite 与 D1 共用 `worker/migrations/*.sql`（Rust 侧 `include_str!` 同一批文件），本地用 `PRAGMA user_version` 记版本。
同步规则：`bars` 与板块系列只增不改、双向 upsert 就是合并；`watchlist` / `settings` 按 `updated_at` 后写者胜；`ai_cache` 不同步。
Worker 不可达时照常看盘，顶栏显示「未同步」。

## 免责声明

本项目仅用于技术学习与个人研究，**不构成任何投资建议**。行情数据来自第三方免费接口，延迟与准确性均无保证，不得用于实盘交易决策。
