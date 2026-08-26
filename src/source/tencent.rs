use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use rust_decimal::Decimal;
use std::str::FromStr;

use crate::core::quote::Quote;
use crate::core::symbol::{Market, Symbol};

pub const SOURCE_ID: &str = "tencent";

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("行格式不对：{0}")]
    BadLine(String),
    #[error("字段数不足，只有 {0} 个")]
    TooFewFields(usize),
    #[error("无法识别的代码前缀：{0}")]
    UnknownPrefix(String),
    #[error("字段 {field} 不是合法数字：{value:?}")]
    BadNumber { field: &'static str, value: String },
    #[error("无法解析时间戳 {0:?}")]
    BadTimestamp(String),
}

/// 关键字段在四个市场里索引一致（总字段数不同：A股 88 / 港股 78 / 美股 71 / 日股 72）
const I_NAME: usize = 1;
const I_LAST: usize = 3;
const I_PREV_CLOSE: usize = 4;
const I_OPEN: usize = 5;
const I_VOLUME: usize = 6;
const I_STAMP: usize = 30;
const I_CHANGE: usize = 31;
const I_CHANGE_PCT: usize = 32;
const I_HIGH: usize = 33;
const I_LOW: usize = 34;
const MIN_FIELDS: usize = 35;

/// 解析一整批响应。返回 Vec<Result> 而不是 Result<Vec> ——
/// 一只股票的脏数据不该让整次刷新失败。
pub fn parse_batch(gbk: &[u8]) -> Vec<Result<Quote, ParseError>> {
    let (text, _, _) = encoding_rs::GBK.decode(gbk);
    text.split(';')
        .map(str::trim)
        .filter(|l| l.starts_with("v_"))
        .map(parse_line)
        .collect()
}

fn parse_line(line: &str) -> Result<Quote, ParseError> {
    let (head, body) = line
        .split_once('=')
        .ok_or_else(|| ParseError::BadLine(line.into()))?;
    let raw_code = head.trim_start_matches("v_").trim();
    let fields: Vec<&str> = body.trim_matches('"').split('~').collect();
    if fields.len() < MIN_FIELDS {
        return Err(ParseError::TooFewFields(fields.len()));
    }

    let symbol = symbol_from_tencent(raw_code)?;
    let tz = symbol.market.quote_timezone();

    Ok(Quote {
        name: fields[I_NAME].trim().to_string(),
        last: num(fields[I_LAST], "现价")?,
        prev_close: num(fields[I_PREV_CLOSE], "昨收")?,
        open: num(fields[I_OPEN], "今开")?,
        high: num(fields[I_HIGH], "最高")?,
        low: num(fields[I_LOW], "最低")?,
        volume: num(fields[I_VOLUME], "成交量")?,
        change: num(fields[I_CHANGE], "涨跌")?,
        change_pct: num(fields[I_CHANGE_PCT], "涨跌幅")?,
        stamped_at: parse_stamp(fields[I_STAMP], tz)?,
        source: SOURCE_ID,
        symbol,
    })
}

fn num(raw: &str, field: &'static str) -> Result<Decimal, ParseError> {
    let s = raw.trim();
    // 日股有大量空字段（78 个字段里近三十个是空的），按 0 处理而不是报错
    if s.is_empty() {
        return Ok(Decimal::ZERO);
    }
    Decimal::from_str(s).map_err(|_| ParseError::BadNumber {
        field,
        value: s.to_string(),
    })
}

/// 把腾讯的查询代码还原成内部 Symbol。
/// 注意源里美股是 AAPL.OQ、日股是 7203.T —— 后缀要剥掉。
fn symbol_from_tencent(raw: &str) -> Result<Symbol, ParseError> {
    // r_hk 必须先于 hk 尝试
    let (market, code) = if let Some(c) = raw.strip_prefix("r_hk").or_else(|| raw.strip_prefix("hk"))
    {
        (Market::Hk, c.to_string())
    } else if let Some(c) = raw.strip_prefix("us") {
        (Market::Us, c.split('.').next().unwrap_or(c).to_string())
    } else if let Some(c) = raw.strip_prefix("jp") {
        (Market::Jp, c.split('.').next().unwrap_or(c).to_string())
    } else if let Some(c) = raw
        .strip_prefix("sh")
        .or_else(|| raw.strip_prefix("sz"))
        .or_else(|| raw.strip_prefix("bj"))
    {
        (Market::Cn, c.to_string())
    } else {
        return Err(ParseError::UnknownPrefix(raw.to_string()));
    };
    Symbol::parse(&format!("{}:{}", market.as_str(), code))
        .map_err(|_| ParseError::UnknownPrefix(raw.to_string()))
}

/// 三种格式：YYYYMMDDHHMMSS（A股）、YYYY/MM/DD HH:MM:SS（港股）、YYYY-MM-DD HH:MM:SS（美/日股）
fn parse_stamp(raw: &str, tz: Tz) -> Result<DateTime<Utc>, ParseError> {
    let s = raw.trim();
    let naive = if s.len() == 14 && s.chars().all(|c| c.is_ascii_digit()) {
        NaiveDateTime::parse_from_str(s, "%Y%m%d%H%M%S")
    } else if s.contains('/') {
        NaiveDateTime::parse_from_str(s, "%Y/%m/%d %H:%M:%S")
    } else {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
    }
    .map_err(|_| ParseError::BadTimestamp(s.to_string()))?;

    let local = tz.from_local_datetime(&naive);
    // 夏令时回拨那一小时里同一个本地时刻会出现两次，single() 会失败。
    // 取较早的那次即可 —— 差一小时不影响新鲜度判断的量级。
    local
        .single()
        .or_else(|| local.earliest())
        .map(|dt| dt.with_timezone(&Utc))
        .ok_or_else(|| ParseError::BadTimestamp(s.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/tencent_batch.txt");

    fn parse_all() -> Vec<Quote> {
        parse_batch(FIXTURE)
            .into_iter()
            .map(|r| r.expect("fixture 必须全部解析成功"))
            .collect()
    }

    #[test]
    fn 解析出全部五只() {
        assert_eq!(parse_all().len(), 5);
    }

    #[test]
    fn gbk中文名不乱码() {
        let qs = parse_all();
        assert_eq!(qs[0].name, "贵州茅台");
        assert_eq!(qs[1].name, "平安银行");
        assert_eq!(qs[2].name, "腾讯控股");
    }

    #[test]
    fn 代码归一回内部格式() {
        let qs = parse_all();
        assert_eq!(qs[0].symbol.to_string(), "CN:600519");
        assert_eq!(qs[2].symbol.to_string(), "HK:00700");
        assert_eq!(qs[3].symbol.to_string(), "US:AAPL"); // 源里是 AAPL.OQ，要剥掉后缀
        assert_eq!(qs[4].symbol.to_string(), "JP:7203"); // 源里是 7203.T
    }

    #[test]
    fn 价格字段解析为decimal() {
        let q = &parse_all()[0];
        assert!(q.last > Decimal::ZERO);
        assert!(q.high >= q.last && q.last >= q.low);
    }

    #[test]
    fn 三种时间戳格式都能解析() {
        for q in parse_all() {
            // 解析出来的时刻必须落在一个合理的年份区间，而不是 epoch 0
            assert!(
                q.stamped_at.timestamp() > 1_700_000_000,
                "{} 时间戳没解析出来",
                q.symbol
            );
        }
    }

    #[test]
    fn 单只损坏不影响其他() {
        let mut bad = b"v_shBROKEN=\"1~x~x~notanumber~\";\n".to_vec();
        bad.extend_from_slice(FIXTURE);
        let results = parse_batch(&bad);
        assert!(results[0].is_err(), "损坏行应该报错");
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            5,
            "其余五只仍应成功"
        );
    }

    #[test]
    fn 空输入返回空() {
        assert!(parse_batch(b"").is_empty());
    }
}
