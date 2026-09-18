//! 历史 K 线接入。
//!
//! 三个市场三个源，日股暂时无源 —— 各自的字段格式与深度差异极大：
//!
//! | 市场 | 源 | 格式 | 深度 | 价格 |
//! |---|---|---|---|---|
//! | A股 | 新浪 `money.finance` | 对象数组 `{day,open,high,low,close,volume}` | 全量（茅台 5990 根） | **原始价** |
//! | 美股 | 新浪 `stock.finance` | 对象数组 `{d,o,h,l,c,v}` | 全量（AAPL 10018 根），**仅日线** | 原始价 |
//! | 港股 | 腾讯 `ifzq` | 嵌套数组 `[日期, 开, 收, 高, 低, 量]` | 640 根 | 前复权（`qfq` 参数） |
//! | 日股 | — | — | **无** | — |
//!
//! A 股这一路给的是**未复权的原始价**，复权靠另一个因子接口（`adj_factors`）
//! 在读库时折算，见 `core::adjust`。
//!
//! 腾讯那个数组的顺序是 **开-收-高-低**，不是 OHLC。照 OHLC 读会把收盘价当最高价。

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;

use crate::core::adjust::AdjFactor;
use crate::core::bar::{Bar, Timeframe};
use crate::core::symbol::{Market, Symbol};
use crate::source::throttle::Throttle;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("{0} 市场的历史 K 线暂无可用免费数据源")]
    MarketUnsupported(&'static str),
    #[error("{source_id} 不支持 {tf} 周期")]
    TimeframeUnsupported {
        source_id: &'static str,
        tf: &'static str,
    },
    #[error("请求失败：{0}")]
    Http(String),
    /// 免费源被限流时返回的是**空响应而不是错误码**，必须当失败处理，
    /// 否则退避永远不会触发，只会被封得更久。
    #[error("响应为空，多半是被限流了")]
    Empty,
    #[error("响应无法解析：{0}")]
    Parse(String),
}

/// 免费源都会因连续请求封 IP，历史请求体积又大，间隔给得比报价更宽。
/// 实测新浪约 10 次快速请求就开始返回空，6-8 秒间隔才稳，取 7 秒。
const MIN_INTERVAL: Duration = Duration::from_secs(7);

pub struct HistoryClient {
    http: reqwest::Client,
    throttle: Throttle,
}

impl HistoryClient {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent("souba/0.1")
                .build()?,
            throttle: Throttle::new(MIN_INTERVAL),
        })
    }

    pub async fn bars(
        &self,
        symbol: &Symbol,
        tf: Timeframe,
        limit: usize,
    ) -> Result<Vec<Bar>, HistoryError> {
        let (url, referer) = endpoint(symbol, tf, limit)?;
        let body = self.get(&url, referer).await?;
        self.finish(&body, symbol.market)
    }

    /// A 股的累计前复权因子。只有新浪有，也只有 A 股有 ——
    /// 港股 K 线本身就是复权后的，美股与日股这条路不存在。
    pub async fn adj_factors(&self, symbol: &Symbol) -> Result<Vec<AdjFactor>, HistoryError> {
        if symbol.market != Market::Cn {
            return Err(HistoryError::MarketUnsupported(symbol.market.as_str()));
        }
        let url = format!(
            "https://finance.sina.com.cn/realstock/company/{}/qfq.js",
            symbol.to_tencent()
        );
        let body = self.get(&url, Some(SINA_REF)).await?;
        let factors = parse_sina_qfq(&body)?;
        if factors.is_empty() {
            self.throttle.penalize();
            return Err(HistoryError::Empty);
        }
        self.throttle.reset();
        Ok(factors)
    }

    /// 节流 + 取字节。HTTP 层的失败都在这里记退避。
    async fn get(&self, url: &str, referer: Option<&str>) -> Result<Vec<u8>, HistoryError> {
        self.throttle.acquire().await;
        let mut req = self.http.get(url);
        if let Some(r) = referer {
            req = req.header("Referer", r);
        }
        let res = req.send().await.map_err(|e| {
            self.throttle.penalize();
            HistoryError::Http(e.to_string())
        })?;
        if !res.status().is_success() {
            self.throttle.penalize();
            return Err(HistoryError::Http(format!("HTTP {}", res.status())));
        }
        res.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| HistoryError::Http(e.to_string()))
    }

    /// 解析 + 判空 + 记账。单拎出来是为了不打真网络也能测「空响应触发退避」。
    fn finish(&self, body: &[u8], market: Market) -> Result<Vec<Bar>, HistoryError> {
        let tz = market.timezone();
        let bars = match market {
            Market::Cn => parse_sina_cn(body, tz),
            Market::Us => parse_sina_us(body, tz),
            Market::Hk => parse_tencent(body, tz),
            Market::Jp => Err(HistoryError::MarketUnsupported("日股")),
        }?;
        if bars.is_empty() {
            self.throttle.penalize();
            return Err(HistoryError::Empty);
        }
        self.throttle.reset();
        Ok(bars)
    }
}

const SINA_REF: &str = "https://finance.sina.com.cn";

/// 返回 (url, referer)
fn endpoint(
    symbol: &Symbol,
    tf: Timeframe,
    limit: usize,
) -> Result<(String, Option<&'static str>), HistoryError> {
    match symbol.market {
        Market::Cn => {
            let scale = match tf {
                Timeframe::Day => 240,
                Timeframe::Week => 1680,
                Timeframe::Month => 7200,
                Timeframe::Min60 => 60,
                Timeframe::Min30 => 30,
                Timeframe::Min15 => 15,
                Timeframe::Min5 => 5,
            };
            Ok((
                format!(
                    "https://money.finance.sina.com.cn/quotes_service/api/json_v2.php/\
                     CN_MarketData.getKLineData?symbol={}&scale={scale}&ma=no&datalen={limit}",
                    symbol.to_tencent()
                ),
                Some(SINA_REF),
            ))
        }
        // 新浪美股接口只有日线，没有分钟线也没有周月线
        Market::Us => match tf {
            Timeframe::Day => Ok((
                format!(
                    "https://stock.finance.sina.com.cn/usstock/api/json_v2.php/\
                     US_MinKService.getDailyK?symbol={}",
                    symbol.code
                ),
                None,
            )),
            other => Err(HistoryError::TimeframeUnsupported {
                source_id: "新浪美股",
                tf: other.label(),
            }),
        },
        Market::Hk => {
            let (host, path) = match tf {
                Timeframe::Day | Timeframe::Week | Timeframe::Month => (
                    "web.ifzq.gtimg.cn",
                    format!(
                        "appstock/app/fqkline/get?param={},{},,,{limit},qfq",
                        hk_kline_code(symbol),
                        match tf {
                            Timeframe::Day => "day",
                            Timeframe::Week => "week",
                            _ => "month",
                        }
                    ),
                ),
                _ => (
                    "ifzq.gtimg.cn",
                    format!(
                        "appstock/app/kline/mkline?param={},m{},,{limit}",
                        hk_kline_code(symbol),
                        match tf {
                            Timeframe::Min60 => 60,
                            Timeframe::Min30 => 30,
                            Timeframe::Min15 => 15,
                            _ => 5,
                        }
                    ),
                ),
            };
            Ok((format!("https://{host}/{path}"), None))
        }
        Market::Jp => Err(HistoryError::MarketUnsupported("日股")),
    }
}

/// K 线接口用的是不带 r_ 的裸前缀 —— r_ 只对实时报价接口有意义
fn hk_kline_code(symbol: &Symbol) -> String {
    format!("hk{}", symbol.code)
}

fn f(s: &str) -> Result<f64, HistoryError> {
    s.trim()
        .parse::<f64>()
        .map_err(|_| HistoryError::Parse(format!("不是数字：{s:?}")))
}

/// 日期串可能是 `YYYY-MM-DD` 或 `YYYY-MM-DD HH:MM:SS`，按市场本地时区归一到 UTC
fn stamp(raw: &str, tz: Tz) -> Result<DateTime<Utc>, HistoryError> {
    let s = raw.trim();
    let naive = if s.len() > 10 {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
            .map_err(|_| HistoryError::Parse(format!("时间戳无法解析：{s:?}")))?
    } else {
        NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map_err(|_| HistoryError::Parse(format!("日期无法解析：{s:?}")))?
            .and_hms_opt(0, 0, 0)
            .expect("00:00:00 必然合法")
    };
    let local = tz.from_local_datetime(&naive);
    local
        .single()
        .or_else(|| local.earliest())
        .map(|d| d.with_timezone(&Utc))
        .ok_or_else(|| HistoryError::Parse(format!("时区换算失败：{s:?}")))
}

/// 新浪 A 股日线 / 分钟线。**返回的是未复权的原始价** —— 落库存的就是它，
/// 复权在读库时用 `adj_factors` 折算。
pub fn parse_sina_cn(body: &[u8], tz: Tz) -> Result<Vec<Bar>, HistoryError> {
    #[derive(serde::Deserialize)]
    struct Row {
        day: String,
        open: String,
        high: String,
        low: String,
        close: String,
        volume: String,
    }
    let rows: Vec<Row> =
        serde_json::from_slice(body).map_err(|e| HistoryError::Parse(e.to_string()))?;
    rows.into_iter()
        .map(|r| {
            Ok(Bar {
                ts: stamp(&r.day, tz)?,
                open: f(&r.open)?,
                high: f(&r.high)?,
                low: f(&r.low)?,
                close: f(&r.close)?,
                volume: f(&r.volume)?,
            })
        })
        .collect()
}

pub fn parse_sina_us(body: &[u8], tz: Tz) -> Result<Vec<Bar>, HistoryError> {
    #[derive(serde::Deserialize)]
    struct Row {
        d: String,
        o: String,
        h: String,
        l: String,
        c: String,
        v: String,
    }
    let rows: Vec<Row> =
        serde_json::from_slice(body).map_err(|e| HistoryError::Parse(e.to_string()))?;
    rows.into_iter()
        // 新浪美股序列里夹着零成交、零价格的占位行（多为停牌或早期数据缺失），丢掉
        .filter(|r| r.c.trim() != "0.00" && r.c.trim() != "0")
        .map(|r| {
            Ok(Bar {
                ts: stamp(&r.d, tz)?,
                open: f(&r.o)?,
                high: f(&r.h)?,
                low: f(&r.l)?,
                close: f(&r.c)?,
                volume: f(&r.v)?,
            })
        })
        .collect()
}

/// 腾讯的嵌套结构：`data.<code>.<period>` 是一个数组的数组。
/// **每行是 [日期, 开, 收, 高, 低, 量]，不是 OHLC。**
pub fn parse_tencent(body: &[u8], tz: Tz) -> Result<Vec<Bar>, HistoryError> {
    let v: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| HistoryError::Parse(e.to_string()))?;
    let rows = find_rows(&v).ok_or_else(|| HistoryError::Parse("响应里找不到 K 线数组".into()))?;
    rows.iter()
        .filter_map(|r| r.as_array())
        .filter(|r| r.len() >= 6)
        .map(|r| {
            let g = |i: usize| -> Result<String, HistoryError> {
                r[i].as_str()
                    .map(str::to_string)
                    .or_else(|| r[i].as_f64().map(|n| n.to_string()))
                    .ok_or_else(|| HistoryError::Parse(format!("字段 {i} 类型意外")))
            };
            Ok(Bar {
                ts: stamp(&g(0)?, tz)?,
                open: f(&g(1)?)?,
                close: f(&g(2)?)?, // 注意：第 2 位是收盘不是最高
                high: f(&g(3)?)?,
                low: f(&g(4)?)?,
                volume: f(&g(5)?)?,
            })
        })
        .collect()
}

/// 新浪 `qfq.js` 的累计前复权因子。响应不是 JSON 而是一段 JS：
/// `var sh600519qfq={"total":33,"data":[{"d":"2026-06-26","f":"1.0000..."},…]}`
/// 后面还跟一段 `/* … */` 的随机注释。源里按除权日**倒序**，
/// 这里统一翻成升序返回 —— `apply_factors` 要靠升序做二分。
pub fn parse_sina_qfq(body: &[u8]) -> Result<Vec<AdjFactor>, HistoryError> {
    #[derive(serde::Deserialize)]
    struct Row {
        d: String,
        f: String,
    }
    #[derive(serde::Deserialize)]
    struct Payload {
        data: Vec<Row>,
    }
    let text = String::from_utf8_lossy(body);
    // 去掉尾部注释再取第一个 `{` 起的对象字面量
    let head = text.split("/*").next().unwrap_or("").trim_end();
    let json = head
        .find('{')
        .map(|i| head[i..].trim_end_matches(';'))
        .ok_or_else(|| HistoryError::Parse("qfq 响应里找不到对象字面量".into()))?;
    let payload: Payload =
        serde_json::from_str(json).map_err(|e| HistoryError::Parse(e.to_string()))?;
    let mut out: Vec<AdjFactor> = payload
        .data
        .into_iter()
        .map(|r| {
            Ok(AdjFactor {
                effective_date: r
                    .d
                    .trim()
                    .parse()
                    .map_err(|_| HistoryError::Parse(format!("除权日无法解析：{:?}", r.d)))?,
                factor: f(&r.f)?,
            })
        })
        .collect::<Result<_, HistoryError>>()?;
    out.sort_by_key(|a| a.effective_date);
    Ok(out)
}

/// 腾讯的周期键名不固定（day / qfqday / m60 …），递归找第一个「数组的数组」
fn find_rows(v: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    match v {
        serde_json::Value::Array(a) if a.first().is_some_and(|x| x.is_array()) => Some(a),
        serde_json::Value::Object(m) => m.values().find_map(find_rows),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CN_DAY: &[u8] = include_bytes!("../../tests/fixtures/sina_cn_kline_day.json");
    const CN_60M: &[u8] = include_bytes!("../../tests/fixtures/sina_cn_kline_60m.json");
    const US_DAY: &[u8] = include_bytes!("../../tests/fixtures/sina_us_kline_day.json");
    const HK_DAY: &[u8] = include_bytes!("../../tests/fixtures/tencent_hk_kline_day.json");
    const QFQ: &[u8] = include_bytes!("../../tests/fixtures/sina_cn_qfq_sh600519.js");

    const SH: Tz = chrono_tz::Asia::Shanghai;
    const NY: Tz = chrono_tz::America::New_York;
    const HK: Tz = chrono_tz::Asia::Hong_Kong;

    fn sane(bars: &[Bar], label: &str) {
        assert!(!bars.is_empty(), "{label}：解析结果为空");
        for (i, b) in bars.iter().enumerate() {
            assert!(b.high >= b.low, "{label} 第 {i} 根 high < low");
            assert!(b.high >= b.open && b.high >= b.close, "{label} 第 {i} 根 high 不是最高");
            assert!(b.low <= b.open && b.low <= b.close, "{label} 第 {i} 根 low 不是最低");
            assert!(b.volume >= 0.0, "{label} 第 {i} 根成交量为负");
            assert!(b.close > 0.0, "{label} 第 {i} 根收盘价非正");
        }
        for w in bars.windows(2) {
            assert!(w[0].ts < w[1].ts, "{label}：时间必须严格递增");
        }
    }

    #[test]
    fn 新浪a股日线() {
        let bars = parse_sina_cn(CN_DAY, SH).unwrap();
        sane(&bars, "新浪A股日线");
        assert_eq!(bars.len(), 60);
    }

    #[test]
    fn 新浪a股分钟线的时间戳带时分秒() {
        let bars = parse_sina_cn(CN_60M, SH).unwrap();
        sane(&bars, "新浪A股60分");
        // 分钟线的 day 字段是 "2026-08-12 15:00:00"，不能当成日期丢掉时分
        let same_day = bars
            .windows(2)
            .any(|w| w[0].ts.date_naive() == w[1].ts.date_naive());
        assert!(same_day, "60 分线一天内应有多根，说明时分秒被解析了");
    }

    #[test]
    fn 新浪美股日线() {
        let bars = parse_sina_us(US_DAY, NY).unwrap();
        sane(&bars, "新浪美股日线");
    }

    #[test]
    fn 腾讯港股日线() {
        let bars = parse_tencent(HK_DAY, HK).unwrap();
        sane(&bars, "腾讯港股日线");
    }

    #[test]
    fn 腾讯字段顺序是开收高低而不是ohlc() {
        // 这是最容易踩的坑：照 OHLC 读会把收盘价当最高价。
        // fixture 第一行原始数据：["2026-06-01","432.400","436.000","442.000","430.0..."]
        // 若按 OHLC 读，high 会变成 436.000，而真正的 high 是 442.000。
        let bars = parse_tencent(HK_DAY, HK).unwrap();
        let b = bars[0];
        assert!(
            (b.open - 432.4).abs() < 1e-6,
            "开盘价应为 432.4，实际 {}",
            b.open
        );
        assert!(
            (b.close - 436.0).abs() < 1e-6,
            "收盘价应为 436.0（第 2 位是收不是高），实际 {}",
            b.close
        );
        assert!(
            (b.high - 442.0).abs() < 1e-6,
            "最高价应为 442.0（第 3 位），实际 {}",
            b.high
        );
    }

    #[test]
    fn 日股明确报出无数据源而不是空数组() {
        let jp = Symbol::parse("JP:7203").unwrap();
        let err = endpoint(&jp, Timeframe::Day, 100).unwrap_err();
        assert!(
            matches!(err, HistoryError::MarketUnsupported(_)),
            "日股应明确报缺源，不能静默返回空 —— 空数组会被当成「这只股票没有历史」"
        );
    }

    #[test]
    fn 美股只支持日线且分钟线明确报错() {
        let us = Symbol::parse("US:AAPL").unwrap();
        assert!(endpoint(&us, Timeframe::Day, 100).is_ok());
        let err = endpoint(&us, Timeframe::Min60, 100).unwrap_err();
        assert!(matches!(err, HistoryError::TimeframeUnsupported { .. }));
    }

    #[test]
    fn 港股k线用裸前缀不用r前缀() {
        // r_ 只对实时报价接口有意义，K 线接口用它会拿不到数据
        let hk = Symbol::parse("HK:00700").unwrap();
        let (url, _) = endpoint(&hk, Timeframe::Day, 100).unwrap();
        assert!(url.contains("param=hk00700,day"), "实际 URL：{url}");
        assert!(!url.contains("r_hk"), "K 线接口不该带 r_ 前缀：{url}");
    }

    #[test]
    fn a股各周期映射到正确的scale() {
        let cn = Symbol::parse("CN:600519").unwrap();
        for (tf, scale) in [
            (Timeframe::Day, "240"),
            (Timeframe::Week, "1680"),
            (Timeframe::Month, "7200"),
            (Timeframe::Min60, "60"),
            (Timeframe::Min5, "5"),
        ] {
            let (url, _) = endpoint(&cn, tf, 100).unwrap();
            assert!(url.contains(&format!("scale={scale}&")), "{} 应映射到 scale={scale}：{url}", tf.label());
        }
    }

    #[test]
    fn 损坏输入报错而不是panic() {
        assert!(parse_sina_cn(b"not json", SH).is_err());
        assert!(parse_tencent(b"{}", HK).is_err());
        assert!(parse_sina_us(b"[]", NY).unwrap().is_empty());
        assert!(parse_sina_qfq(b"not js").is_err());
        assert!(parse_sina_qfq(b"var x={\"data\":[{\"d\":\"whenever\",\"f\":\"1\"}]}").is_err());
    }

    #[test]
    fn qfq因子解析出全部条目并按日期升序() {
        let fs = parse_sina_qfq(QFQ).unwrap();
        assert_eq!(fs.len(), 33, "fixture 的 total 是 33");
        for w in fs.windows(2) {
            assert!(w[0].effective_date < w[1].effective_date, "必须升序且不重复");
        }
        assert_eq!(
            fs.last().unwrap().effective_date,
            "2026-06-26".parse::<chrono::NaiveDate>().unwrap()
        );
        assert!(
            (fs.last().unwrap().factor - 1.0).abs() < 1e-12,
            "最新一条因子必须是 1.0"
        );
        assert!(fs.iter().all(|a| a.factor > 0.0), "因子不能为零或负");
    }

    #[test]
    fn qfq尾部的随机注释不影响解析() {
        let raw = String::from_utf8_lossy(QFQ);
        assert!(raw.contains("/*"), "fixture 应当带着源站那段注释，否则这个测试没意义");
        let trimmed = raw.split("/*").next().unwrap().as_bytes().to_vec();
        assert_eq!(parse_sina_qfq(QFQ).unwrap(), parse_sina_qfq(&trimmed).unwrap());
    }

    #[test]
    fn 空响应触发退避而不是当成没有数据() {
        // 新浪限流时给的就是 `[]`，当成「这只股票没有历史」会把库写空
        let c = HistoryClient::new().unwrap();
        assert!(matches!(c.finish(b"[]", Market::Cn), Err(HistoryError::Empty)));
        assert!(c.throttle.current_backoff() > std::time::Duration::ZERO);
        // 正常响应把退避清掉
        assert!(c.finish(CN_DAY, Market::Cn).is_ok());
        assert_eq!(c.throttle.current_backoff(), std::time::Duration::ZERO);
    }

    /// 在市场判断处就返回了，不会真的发请求
    #[tokio::test]
    async fn 非a股不给因子接口() {
        let c = HistoryClient::new().unwrap();
        let hk = Symbol::parse("HK:00700").unwrap();
        assert!(matches!(
            c.adj_factors(&hk).await,
            Err(HistoryError::MarketUnsupported(_))
        ));
    }
}
