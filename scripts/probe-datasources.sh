#!/usr/bin/env bash
# Probe the free market-data endpoints souba depends on, and report what actually
# comes back. These are undocumented reverse-engineered endpoints — they drift.
# Re-run this before making any architecture decision that rests on them.
#
# Run from a NON-China IP: the backend runs on Cloudflare Workers, so an endpoint
# that only works from inside China is useless to this project.
#
# Usage: ./scripts/probe-datasources.sh

set -uo pipefail

UA='Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/139.0.0.0 Safari/537.36'
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
PASS=0; FAIL=0

hdr() { printf '\n\033[1m== %s\033[0m\n' "$1"; }
ok()  { PASS=$((PASS+1)); printf '  \033[32m✓\033[0m %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  \033[31m✗\033[0m %s\n' "$1"; }

printf 'now: local=%s  beijing=%s  tokyo=%s  newyork=%s\n' \
  "$(date '+%F %T %Z')" "$(TZ=Asia/Shanghai date '+%H:%M:%S')" \
  "$(TZ=Asia/Tokyo date '+%H:%M:%S')" "$(TZ=America/New_York date '+%H:%M:%S')"

# ---------------------------------------------------------------- realtime quotes
hdr 'Tencent qt.gtimg.cn — one endpoint, four markets (GBK encoded)'
echo '  NOTE: for HK the r_ prefix is REALTIME and the bare prefix is 15-min DELAYED.'
echo '        They are twins on the same host — the prefix is the only difference.'
for pair in "sh600519 A-share" "hk00700 HK-delayed" "r_hk00700 HK-REALTIME" "usAAPL US" "jp7203 JP"; do
  set -- ${=pair}
  code=$(curl -s -m 15 -o "$TMP/q" -w '%{http_code}' "https://qt.gtimg.cn/q=$1")
  body=$(iconv -f GBK -t UTF-8 "$TMP/q" 2>/dev/null || cat "$TMP/q")
  # field 31 is the quote timestamp in Tencent's ~-delimited payload
  ts=$(printf '%s' "$body" | cut -d'~' -f31)
  name=$(printf '%s' "$body" | cut -d'~' -f2)
  if [ "$code" = 200 ] && [ -n "$ts" ]; then
    ok "$(printf '%-9s %-6s %-12s ts=%s' "$1" "$2" "$name" "$ts")"
  else
    bad "$(printf '%-9s %-6s HTTP=%s' "$1" "$2" "$code")"
  fi
done
echo '  NOTE: compare ts against the market clock above — HK lags ~15min, JP ~20min.'

hdr 'Sina hq.sinajs.cn — backup source, and the only free REALTIME HK (rt_ prefix)'
for pair in "sh600519 A-share" "gb_aapl US" "hk00700 HK-delay" "rt_hk00700 HK-REALTIME"; do
  set -- ${=pair}
  code=$(curl -s -m 15 -o "$TMP/q" -w '%{http_code}' \
    -H 'Referer: https://finance.sina.com.cn' "https://hq.sinajs.cn/list=$1")
  body=$(iconv -f GBK -t UTF-8 "$TMP/q" 2>/dev/null || cat "$TMP/q")
  if [ "$code" = 200 ] && [ ${#body} -gt 40 ]; then
    ok "$(printf '%-9s %-8s %s' "$1" "$2" "$(printf '%s' "$body" | cut -c1-70)")"
  else
    bad "$(printf '%-9s %-8s HTTP=%s len=%d' "$1" "$2" "$code" "${#body}")"
  fi
done

# ---------------------------------------------------------------- candle history
bars() { # $1=url  -> prints bar count and date range
  curl -s -m 25 -o "$TMP/k" "$1" 2>/dev/null
  python3 - "$TMP/k" <<'PY' 2>/dev/null || echo "0 - -"
import json,sys
try: d=json.load(open(sys.argv[1]))
except Exception: print("0 - -"); raise SystemExit
def walk(o):
    if isinstance(o,dict):
        for v in o.values(): yield from walk(v)
    elif isinstance(o,list) and o and isinstance(o[0],list): yield o
for a in walk(d): print(len(a),a[0][0],a[-1][0]); break
else: print("0 - -")
PY
}

hdr 'Tencent candle history — hard-capped, and no US/JP coverage'
while read -r sym tf n label; do
  if [ "$tf" = day ] || [ "$tf" = week ]; then
    url="https://web.ifzq.gtimg.cn/appstock/app/fqkline/get?param=$sym,$tf,,,$n,qfq"
  else
    url="https://ifzq.gtimg.cn/appstock/app/kline/mkline?param=$sym,$tf,,$n"
  fi
  read -r cnt first last <<<"$(bars "$url")"
  line=$(printf '%-9s %-5s want=%-5s got=%-5s %s..%s  %s' "$sym" "$tf" "$n" "$cnt" "$first" "$last" "$label")
  [ "$cnt" -gt 100 ] && ok "$line" || bad "$line"
done <<'ROWS'
sh600519 day 640 A-share daily
sh600519 day 5000 asking beyond the cap returns EMPTY
sh600519 week 2000 capped at 640 too
sh600519 m60 2000 60min capped at 320
sh600519 m5 640 5min
hk00700 day 640 HK daily
usAAPL day 640 US daily — expect ~2 bars, unusable
jp7203 day 640 JP daily — expect ~1 bar, unusable
ROWS

hdr 'Sina money.finance.sina.com.cn — full history, and what makes EMA576 exact'
echo '  NOTE the host and class name. quotes.sina.cn/CN_MarketDataService caps at 1800 bars;'
echo '  money.finance.sina.com.cn/CN_MarketData returns the symbol ENTIRE history. Use the latter.'
echo '  MUST force HTTP/1.1 — the HTTPS+HTTP/2 path hangs and never returns.'
for spec in "240 6000 daily" "1680 6000 weekly" "60 6000 60min" "5 6000 5min"; do
  set -- ${=spec}
  printf '  scale=%-4s datalen=%-5s %-16s ' "$1" "$2" "$3"
  sleep 6   # these endpoints IP-ban under sustained polling — pace every request
  curl -s -m 40 --http1.1 -o "$TMP/sk" -H "User-Agent: $UA" \
    -H 'Referer: https://finance.sina.com.cn' \
    "https://money.finance.sina.com.cn/quotes_service/api/json_v2.php/CN_MarketData.getKLineData?symbol=sh600519&scale=$1&ma=no&datalen=$2"
  cnt=$(python3 -c "
import json,sys
try: d=json.load(open('$TMP/sk')); print('%d %s %s'%(len(d),d[0]['day'],d[-1]['day']))
except Exception: print('0 - -')
")
  set -- ${=cnt}
  [ "$1" -gt 1000 ] && ok "bars=$1  $2 .. $3" || bad "bars=$1 (empty = unsupported symbol, or rate-limited)"
done
echo '  EMA576 needs ~1330 daily bars for <1% seed error. 5990 bars puts the residue at 9e-10.'

# ---------------------------------------------------------------- known-bad sources
hdr 'Sources already ruled out — confirming they are still bad'
probe_bad() {
  code=$(curl -s -m 15 -o "$TMP/b" -w '%{http_code}' -H "User-Agent: $UA" ${3:+-H "$3"} "$2" 2>/dev/null)
  printf '  %-18s HTTP=%-4s %s\n' "$1" "$code" "$(head -c 60 "$TMP/b" 2>/dev/null | tr -d '\n')"
}
probe_bad Eastmoney 'https://push2his.eastmoney.com/api/qt/stock/kline/get?secid=1.600519&klt=101&lmt=10&fields1=f1&fields2=f51,f52' 'Referer: https://quote.eastmoney.com/'
probe_bad Yahoo-crumb 'https://query2.finance.yahoo.com/v1/test/getcrumb'
probe_bad Yahoo-chart 'https://query1.finance.yahoo.com/v8/finance/chart/AAPL?interval=1d&range=5d'
probe_bad Stooq 'https://stooq.com/q/d/l/?s=aapl.us&i=d'
echo '  Expected: Eastmoney fails or blocks, Yahoo 429s, Stooq returns a JS PoW page.'
echo '  If any of these now WORK, update docs/research/2026-08-26-datasource-probe.md.'

printf '\n\033[1m%d ok, %d failed\033[0m\n' "$PASS" "$FAIL"
