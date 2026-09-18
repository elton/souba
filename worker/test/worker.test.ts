import { env } from 'cloudflare:test'
import { beforeEach, describe, expect, it } from 'vitest'
import worker, { appendDaily, dayTs, dueMarkets, localDay, parseQuotes } from '../src/index'

// tests/fixtures/tencent_batch.txt 里贵州茅台那一行的原始 GBK 字节。
// 内嵌成 base64 而不是读文件：workerd 里没有 fs，而 TextEncoder 也造不出 GBK。
const SH600519_GBK_B64 =
  'dl9zaDYwMDUxOT0iMX6589bdw6nMqH42MDA1MTl+MTMwMy4zOH4xMzA0LjAwfjEzMDAuMDB+MTQ1MTJ+ODIwOX42MzAzfjEzMDMuMjl+MX4xMzAzLjEwfjF+MTMwMy4wM34xfjEzMDMuMDF+Nn4xMzAzLjAwfjR+MTMwMy4zOH40fjEzMDMuNDB+M34xMzAzLjQxfjJ+MTMwMy40Mn4xfjEzMDMuNTN+MTh+fjIwMjYwODI2MTIwNTUyfi0wLjYyfi0wLjA1fjEzMTQuNDV+MTI5NS4wMH4xMzAzLjM4LzE0NTEyLzE4OTc4NDg0MzB+MTQ1MTJ+MTg5Nzg1fjAuMTJ+MjAuMDF+fjEzMTQuNDV+MTI5NS4wMH4xLjQ5fjE2MjkzLjMxfjE2MjkzLjMxfjYuNDh+MTQzNC40MH4xMTczLjYwfjAuODd+LTE1fjEzMDcuODB+MTguMzB+MTkuNzl+fn4wLjEyfjE4OTc4NC44NDMwfjAuMDAwMH4wfiAgIEF+R1AtQX4tMy4zOX4tMC4zNH4zLjk5fjMyLjQxfjI3LjMwfjE1MzkuOTh+MTE1MS4wMX4tMi45NX4tMS4zM34xLjg5fjEyNTAwODE2MDF+MTI1MDA4MTYwMX4tMzYuNTl+LTYuODB+MTI1MDA4MTYwMX5+fi04Ljgzfi0wLjAxfn5DTll+MH5fX19EX19GX19OfjEzMDMuOTh+LTEwfiI7'

function gbkFixture(): Uint8Array {
  const bin = atob(SH600519_GBK_B64)
  return Uint8Array.from(bin, (c) => c.charCodeAt(0))
}

const KEY = 'test-key'

beforeEach(async () => {
  for (const t of [
    'bars',
    'sync_state',
    'watchlist',
    'settings',
    'sectors',
    'sector_daily',
    'sector_members',
    'scan_results',
    'adj_factors',
    'backfill_state',
  ]) {
    await env.DB.exec(`DELETE FROM ${t}`)
  }
})

function post(path: string, body: unknown, key: string | null = KEY) {
  const headers: Record<string, string> = { 'content-type': 'application/json' }
  if (key !== null) headers['X-Souba-Key'] = key
  return new Request(`https://x${path}`, { method: 'POST', headers, body: JSON.stringify(body) })
}

function get(path: string, key: string | null = KEY) {
  const headers: Record<string, string> = {}
  if (key !== null) headers['X-Souba-Key'] = key
  return new Request(`https://x${path}`, { headers })
}

function call(req: Request): Promise<Response> {
  return worker.fetch(req, env)
}

const BAR = {
  symbol: 'CN:600519',
  timeframe: '1d',
  ts: 1756137600,
  open: 1300,
  high: 1314.45,
  low: 1295,
  close: 1303.38,
  volume: 1451200,
}

describe('密钥门锁', () => {
  it('缺少 X-Souba-Key 的写请求返回 401', async () => {
    const res = await call(post('/sync/bars', { bars: [BAR] }, null))
    expect(res.status).toBe(401)
  })

  it('密钥不匹配返回 401 且不写库', async () => {
    const res = await call(post('/sync/bars', { bars: [BAR] }, 'wrong'))
    expect(res.status).toBe(401)
    const { count } = (await env.DB.prepare('SELECT COUNT(*) AS count FROM bars').first<{
      count: number
    }>())!
    expect(count).toBe(0)
  })

  it('读请求同样要求密钥', async () => {
    expect((await call(get('/sync/bars?since=0', null))).status).toBe(401)
  })

  it('密钥正确放行', async () => {
    expect((await call(post('/sync/bars', { bars: [BAR] }))).status).toBe(200)
  })
})

describe('POST /sync/bars', () => {
  it('批量写入后可读回', async () => {
    const res = await call(post('/sync/bars', { bars: [BAR, { ...BAR, ts: BAR.ts + 86400 }] }))
    expect(await res.json()).toEqual({ upserted: 2 })
  })

  it('同一主键重复提交是幂等的，且后写的值生效', async () => {
    await call(post('/sync/bars', { bars: [BAR] }))
    await call(post('/sync/bars', { bars: [{ ...BAR, close: 1310 }] }))
    const rows = await env.DB.prepare('SELECT close FROM bars').all<{ close: number }>()
    expect(rows.results).toEqual([{ close: 1310 }])
  })

  it('空数组不报错', async () => {
    const res = await call(post('/sync/bars', { bars: [] }))
    expect(res.status).toBe(200)
    expect(await res.json()).toEqual({ upserted: 0 })
  })

  it('字段缺失返回 400', async () => {
    const res = await call(post('/sync/bars', { bars: [{ symbol: 'CN:600519' }] }))
    expect(res.status).toBe(400)
  })
})

describe('GET /sync/bars?since=', () => {
  beforeEach(async () => {
    await call(
      post('/sync/bars', {
        bars: [
          { ...BAR, ts: 100 },
          { ...BAR, ts: 200 },
          { ...BAR, ts: 300 },
        ],
      }),
    )
  })

  it('只返回 ts 严格大于 since 的 bar', async () => {
    const body = await (await call(get('/sync/bars?since=200'))).json<{ bars: { ts: number }[] }>()
    expect(body.bars.map((b) => b.ts)).toEqual([300])
  })

  it('since=0 返回全部并按 ts 升序', async () => {
    const body = await (await call(get('/sync/bars?since=0'))).json<{
      bars: { ts: number }[]
      next: number | null
    }>()
    expect(body.bars.map((b) => b.ts)).toEqual([100, 200, 300])
    expect(body.next).toBe(300)
  })

  it('没有更新时返回空数组与 next=null', async () => {
    const body = await (await call(get('/sync/bars?since=300'))).json<{
      bars: unknown[]
      next: number | null
    }>()
    expect(body.bars).toEqual([])
    expect(body.next).toBeNull()
  })

  it('limit 生效，且负数不会变成「不限制」', async () => {
    const one = await (await call(get('/sync/bars?since=0&limit=1'))).json<{ bars: unknown[] }>()
    expect(one.bars).toHaveLength(1)
    // SQLite 里 LIMIT -1 是「不限制」；夹到下界 1，绝不能变成整库倾倒
    const neg = await (await call(get('/sync/bars?since=0&limit=-1'))).json<{ bars: unknown[] }>()
    expect(neg.bars).toHaveLength(1)
  })

  it('缺少 since 返回 400', async () => {
    expect((await call(get('/sync/bars'))).status).toBe(400)
  })

  // 另一台机器回补的新标的，历史 ts 早于本机游标，增量拉永远看不到它 ——
  // 客户端发现「远端已 done、本地没补」时按标的全量拉，靠的就是这个参数。
  it('symbol= 只返回该标的的 bar，且 since=0 时给出全量', async () => {
    await call(post('/sync/bars', { bars: [{ ...BAR, symbol: 'CN:000001', ts: 50 }] }))
    const one = await (await call(get('/sync/bars?since=0&symbol=CN:000001'))).json<{
      bars: { symbol: string; ts: number }[]
      next: number | null
    }>()
    expect(one.bars.map((b) => b.ts)).toEqual([50])
    expect(one.bars.every((b) => b.symbol === 'CN:000001')).toBe(true)
    expect(one.next).toBe(50)
  })

  it('symbol= 与 since= 叠加生效', async () => {
    const body = await (await call(get('/sync/bars?since=100&symbol=CN:600519'))).json<{
      bars: { ts: number }[]
    }>()
    expect(body.bars.map((b) => b.ts)).toEqual([200, 300])
  })

  it('symbol= 对不存在的标的返回空', async () => {
    const body = await (await call(get('/sync/bars?since=0&symbol=CN:999999'))).json<{
      bars: unknown[]
      next: number | null
    }>()
    expect(body.bars).toEqual([])
    expect(body.next).toBeNull()
  })
})

describe('腾讯报价行解析', () => {
  it('GBK 中文名不乱码，字段落在正确的位置', () => {
    const [q] = parseQuotes(gbkFixture())
    expect(q.code).toBe('sh600519')
    expect(q.name).toBe('贵州茅台')
    expect(q.open).toBe(1300)
    expect(q.high).toBe(1314.45)
    expect(q.low).toBe(1295)
    expect(q.close).toBe(1303.38)
    expect(q.volumeLots).toBe(14512)
    expect(q.turnoverWan).toBe(189785)
    expect(q.stamp).toBe('20260826120552')
  })

  it('字段数不足的行被丢弃而不是抛错', () => {
    const broken = new TextEncoder().encode('v_shBAD="1~x~y~";')
    const merged = new Uint8Array(broken.length + gbkFixture().length)
    merged.set(broken)
    merged.set(gbkFixture(), broken.length)
    expect(parseQuotes(merged)).toHaveLength(1)
  })
})

const localDay8 = (d: Date) => localDay(d, 8)
const dayTs8 = (d: string) => dayTs(d, 8)

describe('上海时钟', () => {
  it('把 UTC 时刻换算成上海当日', () => {
    // 2026-08-26 12:05:52 CST = 04:05:52 UTC
    expect(localDay8(new Date('2026-08-26T04:05:52Z'))).toBe('2026-08-26')
    // 跨日：UTC 23:00 已经是上海的次日 07:00
    expect(localDay8(new Date('2026-08-25T23:00:00Z'))).toBe('2026-08-26')
  })

  it('日线 ts 是该交易日上海 00:00 对应的 UTC 秒', () => {
    expect(dayTs8('2026-08-26')).toBe(Date.parse('2026-08-25T16:00:00Z') / 1000)
  })
})

describe('Cron 的市场判定', () => {
  const closed = new Date('2026-09-18T08:00:00Z') // 周五 16:00 CST，收盘一小时后
  const open = new Date('2026-09-18T06:00:00Z') // 周五 14:00 CST，盘中
  const justClosed = new Date('2026-09-18T07:00:00Z') // 周五 15:00 CST，收盘竞价刚落槌

  it('未收盘时不追加', () => {
    expect(dueMarkets(open, {})).toEqual([])
  })

  it('15:00 整点不追加 —— 躲开收盘集合竞价的落槌时刻', () => {
    expect(dueMarkets(justClosed, {})).toEqual([])
  })

  it('已收盘且今日未追加时追加 A 股', () => {
    expect(dueMarkets(closed, {})).toEqual(['CN'])
  })

  it('今日已追加则不重复', () => {
    expect(dueMarkets(closed, { 'last_append:CN': '2026-09-18' })).toEqual([])
  })

  it('记录的是昨天则仍要追加', () => {
    expect(dueMarkets(closed, { 'last_append:CN': '2026-09-17' })).toEqual(['CN'])
  })

  it('周末不追加', () => {
    expect(dueMarkets(new Date('2026-09-19T08:00:00Z'), {})).toEqual([])
  })
})

describe('Cron 追加当日日线', () => {
  it('价格为 0 的停牌行不写库', async () => {
    await call(post('/sync/bars', { bars: [{ ...BAR, ts: 1_700_000_000 }] }))
    // 按字段位造一行「有时间戳但价格全 0」的停牌报价 —— 字段数是够的，
    // 被挡下来必须是因为价格校验，不是因为行太短
    const f = Array<string>(40).fill('0')
    f[1] = 'X'
    f[2] = '600519'
    f[30] = '20260826150000'
    const halted = new TextEncoder().encode(`v_sh600519="${f.join('~')}";`)
    expect(parseQuotes(halted)).toHaveLength(1)

    await appendDaily(env, 'CN', new Date('2026-08-26T08:00:00Z'), async () => new Response(halted))
    const { count } = (await env.DB.prepare('SELECT COUNT(*) AS count FROM bars').first<{
      count: number
    }>())!
    expect(count).toBe(1)
  })

  it('把报价行写成当日原始日线，成交量由手换算成股', async () => {
    // 库里先有这只标的的历史，Cron 才会去追它
    await call(post('/sync/bars', { bars: [{ ...BAR, ts: 1_700_000_000 }] }))

    const fetched: string[] = []
    const fakeFetch = async (url: string) => {
      fetched.push(url)
      return new Response(gbkFixture())
    }

    await appendDaily(env, 'CN', new Date('2026-08-26T08:00:00Z'), fakeFetch)

    expect(fetched[0]).toBe('https://qt.gtimg.cn/q=sh600519')

    const row = await env.DB.prepare(
      'SELECT * FROM bars WHERE ts = ?',
    )
      .bind(dayTs8('2026-08-26'))
      .first<Record<string, number | string>>()
    expect(row).toMatchObject({
      symbol: 'CN:600519',
      timeframe: '1d',
      open: 1300,
      high: 1314.45,
      low: 1295,
      close: 1303.38,
      volume: 1451200, // 14512 手 × 100 —— 与新浪日线的「股」对齐
    })

    const state = await env.DB.prepare('SELECT value FROM sync_state WHERE key = ?')
      .bind('last_append:CN')
      .first<{ value: string }>()
    expect(state?.value).toBe('2026-08-26')
  })

  it('库里没有 A 股标的时不发请求', async () => {
    const fetched: string[] = []
    await appendDaily(env, 'CN', new Date('2026-08-26T08:00:00Z'), async (u) => {
      fetched.push(u)
      return new Response(new Uint8Array())
    })
    expect(fetched).toEqual([])
  })

  it('重跑同一天是幂等的', async () => {
    await call(post('/sync/bars', { bars: [{ ...BAR, ts: 1_700_000_000 }] }))
    const f = async () => new Response(gbkFixture())
    const at = new Date('2026-08-26T08:00:00Z')
    await appendDaily(env, 'CN', at, f)
    await appendDaily(env, 'CN', at, f)
    const { count } = (await env.DB.prepare('SELECT COUNT(*) AS count FROM bars').first<{
      count: number
    }>())!
    expect(count).toBe(2) // 原有那根 + 当日这根
  })
})

describe('Cron 的失败处理', () => {
  it('抓取失败时不记今天，下个整点还会重试', async () => {
    await call(post('/sync/bars', { bars: [{ ...BAR, ts: 1_700_000_000 }] }))
    await appendDaily(
      env,
      'CN',
      new Date('2026-08-26T08:00:00Z'),
      async () => new Response('', { status: 503 }),
    )
    const state = await env.DB.prepare('SELECT value FROM sync_state WHERE key = ?')
      .bind('last_append:CN')
      .first()
    expect(state).toBeNull()
  })
})

// ── 整表交换与板块增量 ──────────────────────────────────────────────────────

function put(path: string, body: unknown, key: string | null = KEY) {
  const headers: Record<string, string> = { 'content-type': 'application/json' }
  if (key !== null) headers['X-Souba-Key'] = key
  return new Request(`https://x${path}`, { method: 'PUT', headers, body: JSON.stringify(body) })
}

const WL = { symbol: 'CN:600519', name: '贵州茅台', sort_order: 1, added_at: 100, updated_at: 200 }

describe('GET|PUT /sync/watchlist', () => {
  it('缺密钥的 GET 与 PUT 都是 401', async () => {
    expect((await call(get('/sync/watchlist', null))).status).toBe(401)
    expect((await call(put('/sync/watchlist', [WL], null))).status).toBe(401)
  })

  it('整表写入后原样读回', async () => {
    expect((await call(put('/sync/watchlist', [WL]))).status).toBe(200)
    const body = await (await call(get('/sync/watchlist'))).json<{ watchlist: unknown[] }>()
    expect(body.watchlist).toEqual([WL])
  })

  it('重复提交幂等', async () => {
    await call(put('/sync/watchlist', [WL]))
    await call(put('/sync/watchlist', [WL]))
    const body = await (await call(get('/sync/watchlist'))).json<{ watchlist: unknown[] }>()
    expect(body.watchlist).toEqual([WL])
  })

  it('updated_at 更新的覆盖旧的', async () => {
    await call(put('/sync/watchlist', [WL]))
    await call(put('/sync/watchlist', [{ ...WL, name: '茅台', updated_at: 300 }]))
    const body = await (await call(get('/sync/watchlist'))).json<{ watchlist: { name: string }[] }>()
    expect(body.watchlist[0].name).toBe('茅台')
  })

  it('updated_at 更旧的不覆盖新的', async () => {
    await call(put('/sync/watchlist', [{ ...WL, updated_at: 300 }]))
    await call(put('/sync/watchlist', [{ ...WL, name: '旧名字', updated_at: 299 }]))
    const body = await (await call(get('/sync/watchlist'))).json<{
      watchlist: { name: string; updated_at: number }[]
    }>()
    expect(body.watchlist[0]).toMatchObject({ name: '贵州茅台', updated_at: 300 })
  })

  it('updated_at 相等时后写者生效', async () => {
    await call(put('/sync/watchlist', [WL]))
    await call(put('/sync/watchlist', [{ ...WL, name: '同刻' }]))
    const body = await (await call(get('/sync/watchlist'))).json<{ watchlist: { name: string }[] }>()
    expect(body.watchlist[0].name).toBe('同刻')
  })

  it('不删远端多出来的行', async () => {
    await call(put('/sync/watchlist', [WL, { ...WL, symbol: 'CN:000001' }]))
    await call(put('/sync/watchlist', [WL]))
    const body = await (await call(get('/sync/watchlist'))).json<{ watchlist: unknown[] }>()
    expect(body.watchlist).toHaveLength(2)
  })

  it('body 不是数组返回 400，字段缺失返回 400', async () => {
    expect((await call(put('/sync/watchlist', { rows: [] }))).status).toBe(400)
    expect((await call(put('/sync/watchlist', [{ symbol: 'CN:600519' }]))).status).toBe(400)
  })

  it('超过单次上限返回 413', async () => {
    const many = Array.from({ length: 2001 }, (_, i) => ({ ...WL, symbol: `CN:${i}` }))
    expect((await call(put('/sync/watchlist', many))).status).toBe(413)
  })

  it('只支持 GET / PUT', async () => {
    expect((await call(post('/sync/watchlist', [WL]))).status).toBe(405)
  })
})

describe('GET|PUT /sync/settings', () => {
  const S = { key: 'ema.fast', value: '144', updated_at: 200 }

  it('缺密钥 401', async () => {
    expect((await call(get('/sync/settings', null))).status).toBe(401)
    expect((await call(put('/sync/settings', [S], null))).status).toBe(401)
  })

  it('整表写入后读回', async () => {
    await call(put('/sync/settings', [S]))
    const body = await (await call(get('/sync/settings'))).json<{ settings: unknown[] }>()
    expect(body.settings).toEqual([S])
  })

  it('updated_at 更旧的不覆盖新的', async () => {
    await call(put('/sync/settings', [{ ...S, updated_at: 300 }]))
    await call(put('/sync/settings', [{ ...S, value: '169', updated_at: 299 }]))
    const body = await (await call(get('/sync/settings'))).json<{ settings: { value: string }[] }>()
    expect(body.settings[0].value).toBe('144')
  })
})

describe('POST|GET /sync/sectors', () => {
  const SECTOR = { market: 'CN', code: 'BK0475', name: '银行', kind: '行业', updated_at: 200 }
  const DAILY = {
    market: 'CN',
    sector_code: 'BK0475',
    date: '2026-09-18',
    change_pct: 1.2,
    turnover: 3.4e10,
  }
  const MEMBER = {
    market: 'CN',
    sector_code: 'BK0475',
    symbol: 'CN:600519',
    name: '贵州茅台',
    as_of: '2026-09-18',
    rank: 1,
  }
  const SCAN = {
    date: '2026-09-18',
    market: 'CN',
    sector_code: 'BK0475',
    symbol: 'CN:600519',
    rank: 1,
    stance: 'Long',
    freshness: 3,
    facets_json: '{}',
  }
  const FACTOR = { symbol: 'CN:600519', effective_date: '2026-06-30', factor: 1.02 }
  const BACKFILL = { symbol: 'CN:600519', timeframe: '1d', status: 'done', updated_at: 200 }

  const ALL = {
    sectors: [SECTOR],
    sector_daily: [DAILY],
    sector_members: [MEMBER],
    scan_results: [SCAN],
    adj_factors: [FACTOR],
    backfill_state: [BACKFILL],
  }

  it('缺密钥 401', async () => {
    expect((await call(post('/sync/sectors', ALL, null))).status).toBe(401)
    expect((await call(get('/sync/sectors?since=0&since_date=1970-01-01', null))).status).toBe(401)
  })

  it('六张表一次写入，全部可读回', async () => {
    expect((await call(post('/sync/sectors', ALL))).status).toBe(200)
    const body = await (
      await call(get('/sync/sectors?since=0&since_date=1970-01-01'))
    ).json<Record<string, unknown>>()
    expect(body.sectors).toEqual([SECTOR])
    expect(body.sector_daily).toEqual([DAILY])
    expect(body.sector_members).toEqual([MEMBER])
    expect(body.scan_results).toEqual([SCAN])
    expect(body.adj_factors).toEqual([FACTOR])
    expect(body.backfill_state).toEqual([BACKFILL])
  })

  it('数组可以缺省', async () => {
    const res = await call(post('/sync/sectors', { sectors: [SECTOR] }))
    expect(res.status).toBe(200)
    expect(await res.json()).toEqual({ upserted: 1 })
  })

  it('同主键重复提交幂等，后写的值生效', async () => {
    await call(post('/sync/sectors', ALL))
    await call(post('/sync/sectors', { sectors: [{ ...SECTOR, name: '银行Ⅱ', updated_at: 300 }] }))
    const body = await (
      await call(get('/sync/sectors?since=0&since_date=1970-01-01'))
    ).json<{ sectors: { name: string }[] }>()
    expect(body.sectors).toEqual([{ ...SECTOR, name: '银行Ⅱ', updated_at: 300 }])
  })

  it('freshness 允许为 null', async () => {
    await call(post('/sync/sectors', { scan_results: [{ ...SCAN, stance: 'Flat', freshness: null }] }))
    const body = await (
      await call(get('/sync/sectors?since=0&since_date=1970-01-01'))
    ).json<{ scan_results: { freshness: number | null }[] }>()
    expect(body.scan_results[0].freshness).toBeNull()
  })

  it('updated_at 游标只回严格更新的行', async () => {
    await call(post('/sync/sectors', ALL))
    const body = await (
      await call(get('/sync/sectors?since=200&since_date=1970-01-01'))
    ).json<Record<string, unknown[]>>()
    expect(body.sectors).toEqual([])
    expect(body.backfill_state).toEqual([])
    // 日期游标的表不受 since 影响
    expect(body.sector_daily).toHaveLength(1)
  })

  it('date 游标按 >= 取，同一天的行会重新回传', async () => {
    await call(post('/sync/sectors', ALL))
    const same = await (
      await call(get('/sync/sectors?since=0&since_date=2026-09-18'))
    ).json<Record<string, unknown[]>>()
    expect(same.sector_daily).toHaveLength(1)
    expect(same.sector_members).toHaveLength(1)
    expect(same.scan_results).toHaveLength(1)

    const later = await (
      await call(get('/sync/sectors?since=0&since_date=2026-09-19'))
    ).json<Record<string, unknown[]>>()
    expect(later.sector_daily).toEqual([])
    expect(later.sector_members).toEqual([])
    expect(later.scan_results).toEqual([])
    // adj_factors 的游标是 effective_date
    expect(later.adj_factors).toEqual([])
  })

  it('next 是两个游标的最大值，没有新行时原样回传请求的游标', async () => {
    await call(post('/sync/sectors', ALL))
    const body = await (
      await call(get('/sync/sectors?since=0&since_date=1970-01-01'))
    ).json<{ next: { since: number; since_date: string } }>()
    expect(body.next).toEqual({ since: 200, since_date: '2026-09-18' })

    const empty = await (
      await call(get('/sync/sectors?since=999&since_date=2030-01-01'))
    ).json<{ next: { since: number; since_date: string } }>()
    expect(empty.next).toEqual({ since: 999, since_date: '2030-01-01' })
  })

  it('总行数超过上限返回 413', async () => {
    const daily = Array.from({ length: 1999 }, (_, i) => ({ ...DAILY, sector_code: `BK${i}` }))
    expect((await call(post('/sync/sectors', { ...ALL, sector_daily: daily }))).status).toBe(413)
  })

  it('未知表名与字段缺失返回 400', async () => {
    expect((await call(post('/sync/sectors', { nope: [{}] }))).status).toBe(400)
    expect((await call(post('/sync/sectors', { sectors: [{ market: 'CN' }] }))).status).toBe(400)
    expect((await call(post('/sync/sectors', { sectors: '不是数组' }))).status).toBe(400)
  })

  it('缺少游标参数返回 400', async () => {
    expect((await call(get('/sync/sectors?since=0'))).status).toBe(400)
    expect((await call(get('/sync/sectors?since_date=1970-01-01'))).status).toBe(400)
    expect((await call(get('/sync/sectors?since=0&since_date=昨天'))).status).toBe(400)
  })

  it('只支持 GET / POST', async () => {
    expect((await call(put('/sync/sectors', ALL))).status).toBe(405)
  })
})
