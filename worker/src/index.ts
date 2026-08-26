interface ProbeResult {
  name: string
  url: string
  ok: boolean
  status: number
  ms: number
  /** GBK 解码后的前 1200 字符，用来肉眼确认中文没乱码 */
  sample: string
  error?: string
}

const TARGETS: { name: string; url: string; headers?: Record<string, string> }[] = [
  { name: 'tencent-quote-batch', url: 'https://qt.gtimg.cn/q=sh600519,r_hk00700,usAAPL,jp7203' },
  { name: 'tencent-kline-day',   url: 'https://web.ifzq.gtimg.cn/appstock/app/fqkline/get?param=sh600519,day,,,640,qfq' },
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
]

async function probe(t: (typeof TARGETS)[number]): Promise<ProbeResult> {
  const started = Date.now()
  try {
    const res = await fetch(t.url, { headers: t.headers ?? {} })
    const buf = await res.arrayBuffer()
    // 腾讯/新浪返回 GBK；K 线接口是 UTF-8 JSON，用 GBK 解也不会炸，只看样本即可
    const sample = new TextDecoder('gbk').decode(buf).slice(0, 1200)
    return {
      name: t.name, url: t.url, ok: res.ok, status: res.status,
      ms: Date.now() - started, sample,
    }
  } catch (e) {
    return {
      name: t.name, url: t.url, ok: false, status: 0,
      ms: Date.now() - started, sample: '',
      error: e instanceof Error ? e.message : String(e),
    }
  }
}

export default {
  async fetch(req: Request): Promise<Response> {
    if (new URL(req.url).pathname !== '/probe') {
      return new Response('GET /probe', { status: 404 })
    }
    const results = await Promise.all(TARGETS.map(probe))
    return Response.json(
      { colo: req.cf?.colo ?? 'unknown', at: new Date().toISOString(), results },
      { headers: { 'cache-control': 'no-store' } },
    )
  },
}
