# CLAUDE.md — souba

Guidance for Claude Code working in this repository. General engineering principles and the
cross-project working mandates live in the parent `../CLAUDE.md` (the shared 01-PWR baseline) and
are inherited automatically — this file only covers what is specific to souba.

## Project Overview

**souba** (相場, "market quotes") is a **terminal-first multi-market stock terminal**: live watchlist
quotes in a TUI, multi-timeframe candles, configurable indicators, pluggable screening strategies,
and an LLM that turns strategy output into recommendations. Human-facing overview: [`README.md`](README.md).

Markets: **A-share, Hong Kong, US, Japan.** Single user — this is a personal tool, not a product.
That decision is load-bearing: **no auth, no multi-tenancy, no quota system.** Do not add them.

**Status: phases 0-2 shipped; phase 3-5 starts with the A-share closed loop.** The design is
[`docs/decisions/2026-08-26-souba-设计.md`](docs/decisions/2026-08-26-souba-设计.md) — read it before
writing anything. Phase 0 is **done and passed**: Tencent and Sina are reachable from a real Worker
(KIX colo, 2-3 ms CPU, GBK decoding works, and the "force HTTP/1.1" trap does not exist there) —
[`docs/research/2026-08-26-worker-出口验证.md`](docs/research/2026-08-26-worker-出口验证.md). Phases 1-2
are in `src/`: A-share watchlist, live TUI refresh, the indicator engine and candle rendering.
The next block is **not** "all four markets" but the **A-share closed loop** —
[`docs/specs/2026-09-18-A股闭环.md`](docs/specs/2026-09-18-A股闭环.md): sector scan, history
persisted locally, Vegas, the opportunity panel, AI reading and D1 sync. Its §一页纸架构 fixes the
cross-phase decisions (schema, adjustment model, one hourly Cron) for HK/US/JP too — do not overturn
them per market. **Worker egress is proven for Tencent, Sina, `finance.yahoo.co.jp` (history page,
`id="histlist"` present, 315 KB), `jpx.co.jp` (`data_j.xlsx`) and GitHub raw** — all 200 on 2026-09-19
via the Worker's `/probe`. `alpaca.markets` needs a key and is only called from the TUI, so it is
verified from the local machine instead (daily bars back to 2016, SIP volume).

## Verified facts that must not be re-derived

These were established by **actually calling the endpoints** on 2026-08-26, plus a second round on
**2026-09-18** covering sector classification, adjustment factors and the per-market history sources
(see [`docs/research/2026-08-26-datasource-probe.md`](docs/research/2026-08-26-datasource-probe.md)
and `scripts/probe-datasources.sh`). Re-run the probe script rather than trusting documentation or
memory — these endpoints are undocumented and drift. **Both rounds ran from a Japanese KDDI
residential/mobile IP, not from a Cloudflare Worker**; only the Tencent and Sina hosts have ever been
called from an actual Worker.

- **Tencent `qt.gtimg.cn` covers all four markets in one endpoint** — prefixes `sh`/`sz`, `hk`, `us`, `jp`.
  Free, no API key, works from overseas IPs. **Responses are GBK, not UTF-8** — decode explicitly.
- **HK has free REALTIME data, behind a prefix.** Tencent `r_hk00700` and Sina `rt_hk00700` return
  quotes 1 second old; the bare `hk00700` twin on the same host is 15-22 min stale. Measured against
  a live session clock, confirmed on both vendors. **Always use the `r_`/`rt_` form for HK.** JP has
  no such twin — `r_jp7203` and `rt_jp7203` both return empty.
- **JP realtime exists only on Yahoo Finance Japan, and only by scraping.** Measured across the TSE
  afternoon open: Yahoo `finance.yahoo.co.jp/quote/7203.T` tracked the live clock (12:34/12:36/12:37,
  price moving) while Tencent `jp7203` sat frozen on the 11:30 morning close. There is no JSON
  endpoint — the data lives in a Next.js RSC flight stream behind build-hashed CSS class names, so it
  **will break when Yahoo redeploys**. Build the JP adapter with a fallback to Tencent's delayed feed
  and surface which one is live in the UI; never let a broken scraper silently show stale prices.
- **Quote latency differs per market and must be computed, not assumed.** A-share and (with the right
  prefix) HK are realtime; JP via Tencent lags 15-20 min and the lag drifts. Derive latency at runtime
  from `response timestamp vs market clock`; never hardcode it. **The TUI must label the real latency
  of every quote.** Presenting delayed data as live is the one product lie this codebase must never tell.
- **Sina has TWO similar-looking kline endpoints and they differ by an order of magnitude.** Use
  `money.finance.sina.com.cn/quotes_service/api/json_v2.php/CN_MarketData.getKLineData` — it returns a
  symbol's ENTIRE history (5990 daily bars for sh600519, back to 2001; 3905 60-min bars; 6000 for the
  SSE index). The other one, `quotes.sina.cn/.../CN_MarketDataService.getKLineData`, caps at 1800.
  **Both must be called with forced HTTP/1.1** — the HTTPS+HTTP/2 path hangs and never returns.
  This solves EMA576 outright for A-share: seed residue at 5990 bars is 9e-10. **A-share only** —
  HK/US/JP symbols all return an empty array.
- **Sina's daily bars are RAW prices — the adjustment factors live somewhere else.** `sh600519` on
  2024-06-14 reads 1555.00 from Sina's `CN_MarketData` vs 1420.66 from Tencent's qfq feed; the gap is
  that year's dividend. Forward-adjustment factors come from the static
  `finance.sina.com.cn/realstock/company/{sh600519}/qfq.js` — `var sh600519qfq={"total":33,"data":[{"d":"2026-06-26","f":"1.0"},...]}`,
  cumulative factors listed newest-ex-date first. Adjusting with them lands within ~0.2% of Tencent's
  qfq (an algorithm difference, not a parsing bug). **Store raw prices plus a factor table and adjust
  on read** — that is what keeps `bars` append-only.
- **Sina has sector endpoints, and they do NOT need a `Referer`** (unlike the kline endpoint above —
  handle headers per endpoint). Industry list `vip.stock.finance.sina.com.cn/q/view/newSinaHy.php`
  and concept/region/legacy-industry `money.finance.sina.com.cn/q/view/newFLJK.php?param=class|area|industry`
  both return **raw GBK bytes** in one shot (code, name, member count, change, turnover). Members come
  from `…/json_v2.php/Market_Center.getHQNodeData?page=1&num=100&sort=changepercent&asc=0&node={code}`
  — JSON with Chinese as `\uXXXX` escapes, **not GBK**, fixed 100 rows per page
  (`Market_Center.getHQNodeStockCount` gives the total). Sector codes like `hangye_ZC27`, `gn_hwqc`,
  `new_blhy` are undocumented — read them from the list endpoint, never hardcode.
  **Tencent has no sector endpoint at all** — `q=bk…` returns `v_pv_none_match`.
- **JP history DOES have a source: Yahoo Japan's *history* page, which is plain server-rendered HTML.**
  `finance.yahoo.co.jp/quote/7203.T/history?from=YYYYMMDD&to=YYYYMMDD` serves a `<table id="histlist">`
  (日付/始値/高値/安値/終値/出来高/調整後終値, 20 rows per page) and the range params jump to any year —
  measured back to 2010. The `<td>` classes carry build hashes, so **locate by `id`, never by class**.
  This is a different page from the *quote* page above, whose RSC-flight fragility still stands.
  **Worker egress to `.co.jp` is verified (2026-09-19, 200, `histlist` present)** — it is not the same WAF as the Yahoo international site.
- **Alpaca's free tier is IEX-only for REALTIME, but SIP-full for HISTORY.** Anything older than 15
  minutes defaults to the full SIP tape unless you pass `feed=iex`; daily bars go back to 2016, 5/15/60-min
  bars are included, 200 requests/min, max 10000 bars per call. Email signup, worldwide, no identity
  check. The earlier "IEX only, ~2-3% of volume" note applied the realtime limit to history and was wrong.
- **Tencent quotes are batched, up to 100 symbols per request, mixing markets freely.**
  `qt.gtimg.cn/q=sh600519,r_hk00700,usAAPL,jp7203` returns all of them in one call. The whole watchlist
  refreshes in a single request regardless of market — do not poll symbol by symbol, and do not split
  by market. This also cuts the IP-ban risk substantially.
- **Every free source IP-bans under sustained polling.** Sina started returning empty after ~10 rapid
  requests and recovered after 6-8s spacing; Eastmoney blocked outright. **The data layer must have
  built-in throttling, caching and exponential backoff.** Never poll these endpoints bare.
- **Eastmoney, NetEase, Yahoo international, and Stooq are all ruled out** — respectively: IP-blocks
  overseas callers; returns 502 overseas; 429s on the first request and is hostile to datacenter IPs
  (which is what a Worker has); and now sits behind a SHA-256 proof-of-work wall.
- **History depth is still the binding constraint outside A-share.** Tencent caps at 640 bars (day/week)
  and 320 (m60), and returns essentially nothing for US/JP. `EMA576` needs ~1330 daily bars for <1% seed
  error. So **history must be accumulated and persisted by us**, not fetched on demand. The 2026-09-18
  round softens this for US (Alpaca, back to 2016) and JP (Yahoo Japan's history page, back to 2010);
  **HK is the one market with no deep source** — 640 Tencent bars, then daily accumulation.

## Cloudflare free-plan constraints that shape the architecture

Full numbers in [`docs/research/2026-08-26-cloudflare-free-limits.md`](docs/research/2026-08-26-cloudflare-free-limits.md).
Three of them are load-bearing:

- **10 ms CPU per invocation applies to Workers, Durable Objects AND Cron Triggers alike** — the DO
  FAQ says verbatim that Durable Objects "have the same per invocation CPU limits as any Workers do".
  Wall-clock is unlimited; CPU is not. Parsing a fat JSON payload alone typically costs 10-20 ms.
  Two consequences: **indicators are computed in the Rust TUI, never on the Worker**; and **cold-start
  history backfill (1800 bars) happens in the TUI too**, which then POSTs batches to the Worker for
  storage. Cron does only the daily incremental append — one new bar per symbol per timeframe, which
  is small enough to fit. A design where Cron backfills history does not work on this plan.
- **KV allows 1,000 writes/day** and is eventually consistent. It cannot cache quotes. Use D1.
- **One always-awake Durable Object consumes ~85% of the daily DO duration budget**, and an outbound
  WebSocket keeps an object alive for at most 15 min anyway. **No persistent DO connection to a data
  vendor.** A plain Worker WebSocket to the TUI is free and unlimited in duration — use that instead.
  (Outgoing DO WebSocket messages are *not* charged; the disqualifier is CPU and duration, not messages.)
- **Worker egress has been verified for Tencent and Sina only** (phase 0, 2026-08-26, KIX colo): six
  calls including two spaced 7 s apart, all 200, no sign of blocking; `TextDecoder('gbk')` works in
  workerd; 2-3 ms CPU even decoding a 647 KB body; and the "force HTTP/1.1" trap does not exist on
  Worker `fetch`. **2026-09-19: `finance.yahoo.co.jp` (history page), `jpx.co.jp` and GitHub raw
  also return 200 from the Worker** — the Yahoo *international* 429 does not carry over to the `.co.jp`
  WAF. Re-run `/probe` (targets in `PROBE_TARGETS`) before trusting a new host. The architecture hedges this — live quotes come
  from the TUI, so Worker IP reputation only affects the daily append path, which can fall back to the
  TUI as well.
- **Cron Triggers are 5 per ACCOUNT**, not per Worker. `eltonzheng-me` already uses one
  (`*/5 * * * *`), so **souba has 4 available**. Budget them deliberately.

The resulting split: the Worker is a **storage and proxy layer, not a compute layer**. The TUI talks
straight to Tencent/Sina for live quotes and computes its own indicators, so it keeps working when the
Worker is down or out of quota.

## The Vegas Tunnel strategy

Codified from the source video; full transcript and derivation in
[`docs/research/vegas-tunnel.md`](docs/research/vegas-tunnel.md).

```
EMA12              filter
EMA144 / EMA169    fast tunnel   — price above it is the only long condition
EMA576 / EMA676    slow tunnel   — governs holding and exit  (= fast tunnel x 4)
```

1. Long only above the tunnel; below it, watch and do nothing.
2. A breakout counts only when **EMA12 crosses above the tunnel too** — price alone is a fake-out.
3. While the slow tunnel keeps sloping up, hold. When it turns down, exit fully.

**The tunnel is a band, not a line.** A single candle piercing it is not a valid breakout — that
band is the whole point, and any implementation that collapses it to one EMA has lost the strategy.

Vegas is the **first** strategy, not the only one. The `Strategy` trait is a deliberate abstraction
(the user chose this over a single concrete implementation) — its shape is in §7 of the design doc and
was checked against MACD-cross, KDJ and Bollinger-breakout so it does not end up shaped around Vegas
alone. `Signal` deliberately does **not** collapse to one score: each facet fails for a different
reason, and that "why" is exactly what gets fed to the LLM.

## Credentials

`.env` (gitignored, mode 600) holds the Cloudflare credentials. Copy from
`../nazoru/.env` — same personal account `10100291fe85c71daed5efb6a3ff7795`.

**The ambient shell `CLOUDFLARE_API_TOKEN` belongs to a different account** and will silently deploy
to or query the wrong place. Never call bare `wrangler`; always source this project's `.env` first.

## Non-negotiables

- **Never present delayed data as realtime.** Every quote carries its source and true age, and the UI
  shows it. This is the product's honesty contract.
- **This tool gives no investment advice.** LLM output is labeled as machine-generated analysis of the
  configured strategy, never as a recommendation to buy or sell.
- **CJK width is a correctness bug, not a polish item.** Stock names are Chinese and Japanese; a TUI
  that measures them in codepoints instead of display cells produces misaligned columns on every row.
