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

**Status: research validated, design not finalized, no product code exists.** Do not scaffold the
application until the design doc in `docs/decisions/` is written and approved.

## Verified facts that must not be re-derived

These were established by **actually calling the endpoints** on 2026-08-26 from a non-China IP
(see [`docs/research/2026-08-26-datasource-probe.md`](docs/research/2026-08-26-datasource-probe.md)
and `scripts/probe-datasources.sh`). Re-run the probe script rather than trusting documentation or
memory — these endpoints are undocumented and drift.

- **Tencent `qt.gtimg.cn` covers all four markets in one endpoint** — prefixes `sh`/`sz`, `hk`, `us`, `jp`.
  Free, no API key, works from overseas IPs. **Responses are GBK, not UTF-8** — decode explicitly.
- **Quote latency differs per market and is not zero.** A-share is genuinely realtime; HK and JP lag
  **15-20 min and the lag drifts** — measured twice, HK 15.0/16.3 min, JP 19.3/16.3 min. So compute
  latency at runtime from `response timestamp vs market clock`; never hardcode it as a constant.
  **The TUI must label the real latency of every quote.** Presenting delayed HK/JP
  data as live is the one product lie this codebase must never tell.
- **Eastmoney (`push2his.eastmoney.com`) is not usable.** HTTPS does not complete from overseas IPs,
  and plain HTTP gets the IP blocked after a handful of requests. Do not add it as a dependency.
- **Yahoo Finance rate-limits hard** (429, and `getcrumb` returns `Too Many Request`). It is hostile
  to datacenter IPs — which is exactly what a Cloudflare Worker has. Treat it as unreliable.
- **Stooq now sits behind a SHA-256 proof-of-work wall.** Not usable.
- **History depth is the binding constraint.** Tencent caps at 640 bars (day/week) and 320 (m60), and
  returns essentially nothing for US/JP history. `EMA576` needs ~1330 daily bars to converge to <1%
  seed error — so **history must be accumulated and persisted by us**, not fetched on demand.

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

Vegas is the **first** strategy, not the only one. Keep the strategy interface pluggable, but do not
invent abstraction for strategies that do not exist yet — one concrete implementation first.

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
