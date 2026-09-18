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
  await env.DB.exec('DELETE FROM bars')
  await env.DB.exec('DELETE FROM sync_state')
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
