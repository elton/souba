/**
 * souba 的 Worker：存储与代理层，不是计算层。
 *
 * - `/sync/bars`：TUI 与 D1 之间的双向 bar 同步。bars 只增不改，所以 upsert 就是合并。
 * - Cron `0 * * * *`：按市场时钟挑出「刚收盘且今日未追加」的市场，用腾讯批量报价
 *   给库里每只标的补上当天那根原始日线。本票只实现 A 股分支。
 *
 * 免费版每次调用只有 10 ms CPU（Cron 也一样），所以这里绝不做指标计算，
 * 报价一批 100 只、D1 用 batch 写。
 */

export interface Env {
  DB: D1Database
  /** 写接口的共享密钥。不是鉴权系统，是门锁。wrangler secret put SOUBA_SYNC_KEY */
  SOUBA_SYNC_KEY: string
}

// ── 时钟 ───────────────────────────────────────────────────────────────────

/**
 * 市场本地日期（YYYY-MM-DD）。
 * A股 / 港股 / 日股所在时区都没有夏令时，固定偏移直接算即可，不必动用 Intl
 * （Intl 的格式化在 10 ms 预算里并不便宜）。美股进来时这里要单独处理。
 */
export function localDay(at: Date, offsetHours: number): string {
  return new Date(at.getTime() + offsetHours * 3_600_000).toISOString().slice(0, 10)
}

/**
 * 日线的 ts = 该交易日在市场本地时区 00:00 对应的 UTC epoch 秒。
 * 与 Rust 侧 `parse_sina_cn` 的口径一致 —— 两边算出来必须是同一个数，否则同步会写出重复的 bar。
 */
export function dayTs(day: string, offsetHours: number): number {
  return (Date.parse(`${day}T00:00:00Z`) - offsetHours * 3_600_000) / 1000
}

// ── 市场 ───────────────────────────────────────────────────────────────────

interface MarketSpec {
  /** 相对 UTC 的固定时区偏移（小时） */
  offsetHours: number
  /** 追加时刻（市场本地整点）；到点之后当天的报价行就等于当日日线 */
  appendHour: number
  /** 内部代码 → 腾讯查询代码 */
  vendorCode(code: string): string
  /** 腾讯报价的成交量单位换算到库里的单位（A股报价是「手」，新浪日线是「股」） */
  volumeScale: number
}

/** A股交易所由代码段决定，与 Rust 侧 `cn_prefix` 保持一致 */
function cnVendorCode(code: string): string {
  if (code.startsWith('92')) return `bj${code}` // 北交所 920xxx 也是 9 开头，必须先判
  const head = code[0]
  if (head === '6' || head === '9') return `sh${code}`
  if (head === '4' || head === '8') return `bj${code}`
  return `sz${code}`
}

/**
 * 本票只实现 A 股。港美日按同样的形状各加一条即可 ——
 * 美股有夏令时，加进来时 `offsetHours` 要换成按时区实算。
 */
const MARKETS: Record<string, MarketSpec> = {
  // A股 14:57–15:00 是收盘集合竞价，15:00 整点那一刻的报价可能还是竞价前的价。
  // 取 16 点而不是 15 点：一根日线晚一小时落库没人在意，写错收盘价有人在意。
  CN: { offsetHours: 8, appendHour: 16, vendorCode: cnVendorCode, volumeScale: 100 },
}

const SYNC_STATE_PREFIX = 'last_append:'

/**
 * 该追加哪些市场：本地时间已过收盘点、是工作日、且 `sync_state` 里今天还没记过。
 *
 * 节假日不判断（需要交易日历，本票不做）：休市日腾讯报价行的时间戳仍是上一个交易日，
 * 于是写回去的就是同一根 bar 的重复 upsert，幂等无害。
 */
export function dueMarkets(now: Date, state: Record<string, string>): string[] {
  return Object.entries(MARKETS)
    .filter(([market, spec]) => {
      const local = new Date(now.getTime() + spec.offsetHours * 3_600_000)
      const weekday = local.getUTCDay()
      if (weekday === 0 || weekday === 6) return false
      if (local.getUTCHours() < spec.appendHour) return false
      return state[SYNC_STATE_PREFIX + market] !== localDay(now, spec.offsetHours)
    })
    .map(([market]) => market)
}

// ── 腾讯批量报价 ───────────────────────────────────────────────────────────

/** 字段位与 Rust 侧 `parse_batch` 对齐；四个市场索引一致，只是总字段数不同 */
const I_NAME = 1
const I_LAST = 3
const I_OPEN = 5
const I_VOLUME = 6
const I_STAMP = 30
const I_HIGH = 33
const I_LOW = 34
const I_TURNOVER = 37
const MIN_FIELDS = 38

export interface QuoteRow {
  /** 腾讯查询代码，如 sh600519 */
  code: string
  name: string
  open: number
  high: number
  low: number
  /** 收盘之后「现价」就是当日收盘价 */
  close: number
  /** 成交量，单位「手」 */
  volumeLots: number
  /** 成交额，单位「万元」。bars 表不存它，解析出来只为核对与将来备用 */
  turnoverWan: number
  /** A股是 YYYYMMDDHHMMSS */
  stamp: string
}

function num(raw: string): number {
  const v = Number.parseFloat(raw)
  // 空字段（日股大量存在）按 0 处理，与 Rust 侧一致
  return Number.isFinite(v) ? v : 0
}

/** 响应是 GBK，不是 UTF-8。一只脏数据不该让整批刷新失败，坏行直接丢。 */
export function parseQuotes(gbk: Uint8Array | ArrayBuffer): QuoteRow[] {
  const text = new TextDecoder('gbk').decode(gbk)
  const out: QuoteRow[] = []
  for (const raw of text.split(';')) {
    const line = raw.trim()
    if (!line.startsWith('v_')) continue
    const eq = line.indexOf('=')
    if (eq < 0) continue
    const f = line.slice(eq + 1).replace(/^"/, '').replace(/"$/, '').split('~')
    if (f.length < MIN_FIELDS) continue
    out.push({
      code: line.slice(2, eq).trim(),
      name: f[I_NAME].trim(),
      open: num(f[I_OPEN]),
      high: num(f[I_HIGH]),
      low: num(f[I_LOW]),
      close: num(f[I_LAST]),
      volumeLots: num(f[I_VOLUME]),
      turnoverWan: num(f[I_TURNOVER]),
      stamp: f[I_STAMP].trim(),
    })
  }
  return out
}

// ── D1 ─────────────────────────────────────────────────────────────────────

const UPSERT_BAR = `INSERT INTO bars (symbol, timeframe, ts, open, high, low, close, volume)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
ON CONFLICT (symbol, timeframe, ts) DO UPDATE SET
  open = excluded.open, high = excluded.high, low = excluded.low,
  close = excluded.close, volume = excluded.volume`

/** 一次 D1 batch 里的语句数；再大就会顶到请求体与 CPU 上限 */
const BATCH = 500
/** 单次 POST 允许的 bar 数。回补 5990 根要由客户端自己切片推上来。 */
const MAX_POST_BARS = 2000
/** GET 单页上限 */
const PAGE = 5000

async function writeInBatches(env: Env, stmts: D1PreparedStatement[]): Promise<void> {
  for (let i = 0; i < stmts.length; i += BATCH) {
    await env.DB.batch(stmts.slice(i, i + BATCH))
  }
}

// ── Cron ───────────────────────────────────────────────────────────────────

/** 腾讯一次最多 100 只，也是 10 ms CPU 下解码 + 写库的安全线；观测到逼近 10 ms 就降到 50 */
const QUOTE_BATCH = 100

/**
 * 给某个市场库里的每只标的追加当天的原始日线。
 * `doFetch` 可注入，测试用固定的报价字节跑完整条写库路径。
 */
export async function appendDaily(
  env: Env,
  market: string,
  now: Date,
  doFetch: (url: string) => Promise<Response> = (u) => fetch(u),
): Promise<number> {
  const spec = MARKETS[market]
  if (!spec) throw new Error(`未实现的市场：${market}`)

  const { results } = await env.DB.prepare(
    `SELECT DISTINCT symbol FROM bars WHERE timeframe = '1d' AND symbol LIKE ?1`,
  )
    .bind(`${market}:%`)
    .all<{ symbol: string }>()

  const upsert = env.DB.prepare(UPSERT_BAR)
  let written = 0
  let allOk = true

  for (let i = 0; i < results.length; i += QUOTE_BATCH) {
    const slice = results.slice(i, i + QUOTE_BATCH)
    // 腾讯代码 → 内部 symbol，避免在这边再实现一遍前缀的反向映射
    const bySymbol = new Map(slice.map((r) => [spec.vendorCode(r.symbol.split(':')[1]), r.symbol]))
    const res = await doFetch(`https://qt.gtimg.cn/q=${[...bySymbol.keys()].join(',')}`)
    if (!res.ok) {
      allOk = false
      continue
    }

    const stmts: D1PreparedStatement[] = []
    for (const q of parseQuotes(await res.arrayBuffer())) {
      const symbol = bySymbol.get(q.code)
      // 停牌 / 脏行：任一价格为 0，或时间戳不是 YYYYMMDDHHMMSS —— 宁可不写也不写垃圾进去
      const priced = q.open > 0 && q.high > 0 && q.low > 0 && q.close > 0
      if (!symbol || !priced || !/^\d{14}$/.test(q.stamp)) continue
      const day = `${q.stamp.slice(0, 4)}-${q.stamp.slice(4, 6)}-${q.stamp.slice(6, 8)}`
      stmts.push(
        upsert.bind(
          symbol,
          '1d',
          dayTs(day, spec.offsetHours),
          q.open,
          q.high,
          q.low,
          q.close,
          q.volumeLots * spec.volumeScale,
        ),
      )
    }
    await writeInBatches(env, stmts)
    written += stmts.length
  }

  // 有任何一批没拿到就不记今天，让下个整点的 Cron 重跑 ——
  // 重跑对已写进去的那些是幂等的，而记早了这一天就永远补不回来了。
  if (allOk) {
    await env.DB.prepare(
      `INSERT INTO sync_state (key, value) VALUES (?1, ?2)
       ON CONFLICT (key) DO UPDATE SET value = excluded.value`,
    )
      .bind(SYNC_STATE_PREFIX + market, localDay(now, spec.offsetHours))
      .run()
  }

  return written
}

async function runCron(env: Env, now: Date): Promise<void> {
  const { results } = await env.DB.prepare(
    `SELECT key, value FROM sync_state WHERE key LIKE '${SYNC_STATE_PREFIX}%'`,
  ).all<{ key: string; value: string }>()
  const state = Object.fromEntries(results.map((r) => [r.key, r.value]))
  for (const market of dueMarkets(now, state)) {
    await appendDaily(env, market, now)
  }
}

// ── 路由 ───────────────────────────────────────────────────────────────────

function bad(msg: string, status = 400): Response {
  return Response.json({ error: msg }, { status })
}

interface BarInput {
  symbol: string
  timeframe: string
  ts: number
  open: number
  high: number
  low: number
  close: number
  volume: number
}

const NUMERIC_FIELDS = ['ts', 'open', 'high', 'low', 'close', 'volume'] as const

function asBar(v: unknown): BarInput | null {
  const b = v as Record<string, unknown> | null
  if (!b || typeof b.symbol !== 'string' || typeof b.timeframe !== 'string') return null
  for (const k of NUMERIC_FIELDS) {
    if (typeof b[k] !== 'number' || !Number.isFinite(b[k])) return null
  }
  return b as unknown as BarInput
}

async function postBars(req: Request, env: Env): Promise<Response> {
  let body: { bars?: unknown }
  try {
    body = await req.json()
  } catch {
    return bad('请求体不是合法 JSON')
  }
  const bars = body?.bars
  if (!Array.isArray(bars)) return bad('缺少 bars 数组')
  if (bars.length > MAX_POST_BARS) return bad(`一次最多 ${MAX_POST_BARS} 根，请分片推送`, 413)

  const upsert = env.DB.prepare(UPSERT_BAR)
  const stmts: D1PreparedStatement[] = []
  for (const raw of bars) {
    const b = asBar(raw)
    if (!b) return bad('bar 字段缺失或类型不对')
    stmts.push(upsert.bind(b.symbol, b.timeframe, b.ts, b.open, b.high, b.low, b.close, b.volume))
  }
  await writeInBatches(env, stmts)
  return Response.json({ upserted: stmts.length })
}

async function getBars(url: URL, env: Env): Promise<Response> {
  const raw = url.searchParams.get('since')
  if (raw === null) return bad('缺少 since 参数')
  const since = Number(raw)
  if (!Number.isFinite(since)) return bad('since 不是数字')
  // 夹在 1..PAGE：SQLite 的负数 LIMIT 等于不限制，那就成了无界响应
  const limit = Math.min(Math.max(Number(url.searchParams.get('limit')) || PAGE, 1), PAGE)
  // 可选的按标的过滤：另一台机器回补的新标的，历史 ts 早于本机游标，
  // 光靠 since 永远拉不到它 —— 客户端此时带 symbol= 且 since=0 全量拉这一只。
  const symbol = url.searchParams.get('symbol')

  const { results } = await env.DB.prepare(
    `SELECT symbol, timeframe, ts, open, high, low, close, volume
     FROM bars WHERE ts > ?1${symbol === null ? '' : ' AND symbol = ?3'}
     ORDER BY ts LIMIT ?2`,
  )
    .bind(...(symbol === null ? [since, limit] : [since, limit, symbol]))
    .all<{ ts: number }>()

  let rows = results
  // 满页时同一个 ts 的行可能被 LIMIT 从中间切断，丢掉末尾那组，
  // 客户端用 next 续拉才不会漏。整页同一个 ts 时不裁，否则就空转了。
  if (rows.length === limit) {
    const lastTs = rows[rows.length - 1].ts
    const trimmed = rows.filter((r) => r.ts !== lastTs)
    if (trimmed.length > 0) rows = trimmed
  }
  return Response.json({ bars: rows, next: rows.length ? rows[rows.length - 1].ts : null })
}

// ── 整表交换与板块增量 ─────────────────────────────────────────────────────

/** 列名一律加引号：sector_members / scan_results 里的 `rank` 是 SQLite 的窗口函数名 */
function q(ident: string): string {
  return `"${ident}"`
}

interface TableDef {
  /** 全部列，也是 GET 回传与 POST 必填的字段 */
  cols: readonly string[]
  /** 主键列，用于 ON CONFLICT */
  keys: readonly string[]
}

/** 唯一允许为 NULL 的列（0002 里 scan_results.freshness 之外全是 NOT NULL） */
const NULLABLE = new Set(['freshness'])

/** `updated_at >= 库里那行` 才覆盖：后写者胜的规则在服务端也守一遍 */
const FULL_TABLES: Record<string, TableDef> = {
  watchlist: {
    cols: ['symbol', 'name', 'sort_order', 'added_at', 'updated_at'],
    keys: ['symbol'],
  },
  settings: { cols: ['key', 'value', 'updated_at'], keys: ['key'] },
}

/**
 * `/sync/sectors` 交换的六张表。`cursor` 是增量游标列：
 * 有 `updated_at` 的按 unix 秒严格大于；0002 里没给 `updated_at` 的表只能拿业务日期
 * 当游标，按 `>=` 取 —— 同一天的行会重复回传，而 upsert 幂等，重传无害、漏传有害。
 */
const SECTOR_TABLES: Record<string, TableDef & { cursor: string }> = {
  sectors: {
    cols: ['market', 'code', 'name', 'kind', 'updated_at'],
    keys: ['market', 'code'],
    cursor: 'updated_at',
  },
  sector_daily: {
    cols: ['market', 'sector_code', 'date', 'change_pct', 'turnover'],
    keys: ['market', 'sector_code', 'date'],
    cursor: 'date',
  },
  sector_members: {
    cols: ['market', 'sector_code', 'symbol', 'name', 'as_of', 'rank'],
    keys: ['market', 'sector_code', 'as_of', 'symbol'],
    cursor: 'as_of',
  },
  scan_results: {
    cols: ['date', 'market', 'sector_code', 'symbol', 'rank', 'stance', 'freshness', 'facets_json'],
    keys: ['date', 'market', 'sector_code', 'symbol'],
    cursor: 'date',
  },
  adj_factors: {
    cols: ['symbol', 'effective_date', 'factor'],
    keys: ['symbol', 'effective_date'],
    cursor: 'effective_date',
  },
  backfill_state: {
    cols: ['symbol', 'timeframe', 'status', 'updated_at'],
    keys: ['symbol', 'timeframe'],
    cursor: 'updated_at',
  },
}

function upsertSql(table: string, def: TableDef): string {
  const set = def.cols
    .filter((c) => !def.keys.includes(c))
    .map((c) => `${q(c)} = excluded.${q(c)}`)
    .join(', ')
  return `INSERT INTO ${q(table)} (${def.cols.map(q).join(', ')})
VALUES (${def.cols.map((_, i) => `?${i + 1}`).join(', ')})
ON CONFLICT (${def.keys.map(q).join(', ')}) DO UPDATE SET ${set}`
}

/** 字段齐了才绑；缺一列或类型不对就整批拒绝，宁可 400 也不往库里写半行 */
function bindRow(
  stmt: D1PreparedStatement,
  cols: readonly string[],
  raw: unknown,
): D1PreparedStatement | null {
  if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) return null
  const r = raw as Record<string, unknown>
  const vals: (string | number | null)[] = []
  for (const c of cols) {
    const v = r[c]
    if (v === null || v === undefined) {
      if (!NULLABLE.has(c)) return null
      vals.push(null)
    } else if (typeof v === 'string' || (typeof v === 'number' && Number.isFinite(v))) {
      vals.push(v)
    } else {
      return null
    }
  }
  return stmt.bind(...vals)
}

async function readJson(req: Request): Promise<unknown> {
  try {
    return await req.json()
  } catch {
    return undefined
  }
}

const DATE_RE = /^\d{4}-\d{2}-\d{2}$/

// ── /sync/watchlist、/sync/settings ────────────────────────────────────────

async function getFullTable(table: string, env: Env): Promise<Response> {
  const def = FULL_TABLES[table]
  const { results } = await env.DB.prepare(
    `SELECT ${def.cols.map(q).join(', ')} FROM ${q(table)} ORDER BY ${q(def.keys[0])}`,
  ).all()
  return Response.json({ [table]: results })
}

async function putFullTable(table: string, req: Request, env: Env): Promise<Response> {
  const def = FULL_TABLES[table]
  const rows = await readJson(req)
  if (!Array.isArray(rows)) return bad(`${table} 的 body 必须是整表数组`)
  if (rows.length > MAX_POST_BARS) return bad(`一次最多 ${MAX_POST_BARS} 行，请分片推送`, 413)

  // 传入行的 updated_at 不新于库里那行就不覆盖 —— 客户端已经合并过一次，
  // 这里是第二道：两台机器乱序推上来时，旧值不会盖掉新值。
  const upsert = env.DB.prepare(`${upsertSql(table, def)}
WHERE excluded."updated_at" >= ${q(table)}."updated_at"`)
  const stmts: D1PreparedStatement[] = []
  for (const raw of rows) {
    const stmt = bindRow(upsert, def.cols, raw)
    if (!stmt) return bad(`${table} 行字段缺失或类型不对`)
    stmts.push(stmt)
  }
  await writeInBatches(env, stmts)
  return Response.json({ upserted: stmts.length })
}

// ── /sync/sectors ──────────────────────────────────────────────────────────

async function postSectors(req: Request, env: Env): Promise<Response> {
  const body = await readJson(req)
  if (typeof body !== 'object' || body === null || Array.isArray(body)) {
    return bad('请求体必须是按表名分组的对象')
  }
  const groups = body as Record<string, unknown>
  for (const table of Object.keys(groups)) {
    if (!SECTOR_TABLES[table]) return bad(`未知的表：${table}`)
    if (!Array.isArray(groups[table])) return bad(`${table} 必须是数组`)
  }
  const total = Object.values(groups).reduce<number>((n, rows) => n + (rows as unknown[]).length, 0)
  if (total > MAX_POST_BARS) return bad(`一次最多 ${MAX_POST_BARS} 行，请分片推送`, 413)

  const stmts: D1PreparedStatement[] = []
  for (const [table, def] of Object.entries(SECTOR_TABLES)) {
    const rows = groups[table]
    if (!Array.isArray(rows)) continue
    const upsert = env.DB.prepare(upsertSql(table, def))
    for (const raw of rows) {
      const stmt = bindRow(upsert, def.cols, raw)
      if (!stmt) return bad(`${table} 行字段缺失或类型不对`)
      stmts.push(stmt)
    }
  }
  await writeInBatches(env, stmts)
  return Response.json({ upserted: stmts.length })
}

async function getSectors(url: URL, env: Env): Promise<Response> {
  const rawSince = url.searchParams.get('since')
  const sinceDate = url.searchParams.get('since_date')
  const since = Number(rawSince)
  if (rawSince === null || !Number.isFinite(since)) return bad('since 缺失或不是数字')
  if (sinceDate === null || !DATE_RE.test(sinceDate)) return bad('since_date 缺失或不是 YYYY-MM-DD')

  const out: Record<string, unknown> = {}
  let nextSince = since
  let nextDate = sinceDate

  for (const [table, def] of Object.entries(SECTOR_TABLES)) {
    const unix = def.cursor === 'updated_at'
    const { results } = await env.DB.prepare(
      `SELECT ${def.cols.map(q).join(', ')} FROM ${q(table)}
       WHERE ${q(def.cursor)} ${unix ? '>' : '>='} ?1 ORDER BY ${q(def.cursor)} LIMIT ?2`,
    )
      .bind(unix ? since : sinceDate, PAGE)
      .all<Record<string, string | number | null>>()

    let rows = results
    // unix 游标是严格大于，满页时末尾同一秒的一组可能被 LIMIT 从中间切断，
    // 客户端用 next 续拉就会跳过它 —— 整组丢掉让下一页重取。
    // 整页都是同一秒时不裁，否则就空转了。date 游标是 >=，不存在这个问题。
    if (unix && rows.length === PAGE) {
      const last = rows[rows.length - 1][def.cursor]
      const trimmed = rows.filter((r) => r[def.cursor] !== last)
      if (trimmed.length > 0) rows = trimmed
    }
    out[table] = rows

    for (const r of rows) {
      const v = r[def.cursor]
      if (unix) nextSince = Math.max(nextSince, v as number)
      else nextDate = (v as string) > nextDate ? (v as string) : nextDate
    }
  }

  // 没有新行时原样回传请求的游标，客户端无脑存下来即可
  return Response.json({ ...out, next: { since: nextSince, since_date: nextDate } })
}

// 阶段 0 的出口探测保留下来：下一块要从 Worker 验证 finance.yahoo.co.jp 的可达性，
// 改 TARGETS 即可，不用再搭一遍。
const PROBE_TARGETS: { name: string; url: string; headers?: Record<string, string>; marker?: string }[] = [
  { name: 'tencent-quote-batch', url: 'https://qt.gtimg.cn/q=sh600519,r_hk00700,usAAPL,jp7203' },
  {
    name: 'tencent-kline-day',
    url: 'https://web.ifzq.gtimg.cn/appstock/app/fqkline/get?param=sh600519,day,,,640,qfq',
  },
  {
    name: 'sina-quote',
    url: 'https://hq.sinajs.cn/list=sh600519',
    headers: { Referer: 'https://finance.sina.com.cn' },
  },
  {
    name: 'sina-kline-full',
    url: 'https://money.finance.sina.com.cn/quotes_service/api/json_v2.php/CN_MarketData.getKLineData?symbol=sh600519&scale=240&ma=no&datalen=6000',
    headers: { Referer: 'https://finance.sina.com.cn' },
  },
  // 下面三个是港美日那一块要用的源，2026-09-19 加进来验 Worker 出口可达性
  {
    name: 'yahoo-jp-history',
    url: 'https://finance.yahoo.co.jp/quote/7203.T/history?from=20200101&to=20200131',
    marker: 'id="histlist"',
  },
  {
    name: 'jpx-data-j-xlsx',
    url: 'https://www.jpx.co.jp/markets/statistics-equities/misc/tvdivq0000001vg2-att/data_j.xlsx',
  },
  {
    name: 'sp500-gics-csv',
    url: 'https://raw.githubusercontent.com/datasets/s-and-p-500-companies/main/data/constituents.csv',
  },
]

async function probe(req: Request): Promise<Response> {
  const results = await Promise.all(
    PROBE_TARGETS.map(async (t) => {
      const started = Date.now()
      try {
        const res = await fetch(t.url, { headers: t.headers ?? {} })
        const buf = await res.arrayBuffer()
        return {
          name: t.name,
          url: t.url,
          ok: res.ok,
          status: res.status,
          ms: Date.now() - started,
          bytes: buf.byteLength,
          // 只截样本看编码；整页有没有目标标记另外用 marker 判断，免得样本塞爆响应
          marker: t.marker ? new TextDecoder('utf-8').decode(buf).includes(t.marker) : undefined,
          sample: new TextDecoder('gbk').decode(buf).slice(0, 1200),
        }
      } catch (e) {
        return {
          name: t.name,
          url: t.url,
          ok: false,
          status: 0,
          ms: Date.now() - started,
          sample: '',
          error: e instanceof Error ? e.message : String(e),
        }
      }
    }),
  )
  return Response.json(
    { colo: req.cf?.colo ?? 'unknown', at: new Date().toISOString(), results },
    { headers: { 'cache-control': 'no-store' } },
  )
}

export default {
  async fetch(req: Request, env: Env): Promise<Response> {
    // 读写一律要密钥。spec 只要求写接口带，但读接口开着等于把整个库敞开，
    // 客户端本来每个请求都带这把钥匙，收紧不花成本。
    if (req.headers.get('X-Souba-Key') !== env.SOUBA_SYNC_KEY) {
      return bad('X-Souba-Key 不匹配', 401)
    }
    const url = new URL(req.url)
    if (url.pathname === '/sync/bars') {
      if (req.method === 'POST') return postBars(req, env)
      if (req.method === 'GET') return getBars(url, env)
      return bad('只支持 GET / POST', 405)
    }
    if (url.pathname === '/sync/watchlist' || url.pathname === '/sync/settings') {
      const table = url.pathname.slice('/sync/'.length)
      if (req.method === 'GET') return getFullTable(table, env)
      if (req.method === 'PUT') return putFullTable(table, req, env)
      return bad('只支持 GET / PUT', 405)
    }
    if (url.pathname === '/sync/sectors') {
      if (req.method === 'POST') return postSectors(req, env)
      if (req.method === 'GET') return getSectors(url, env)
      return bad('只支持 GET / POST', 405)
    }
    if (url.pathname === '/probe') return probe(req)
    return bad('未知路径', 404)
  },

  async scheduled(_controller: ScheduledController, env: Env): Promise<void> {
    await runCron(env, new Date())
  },
}
