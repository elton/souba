//! 新浪的行业 / 概念板块列表。
//!
//! 两个端点返回的是同一种东西：`var S_Finance_bankuai_xxx = { "代码": "逗号分隔的一行", ... }`，
//! **GBK 原始字节**，一次吐全。`=` 之后的部分是合法 JSON 对象，所以先解 JSON 再拆逗号。
//!
//! 板块代码（`new_blhy` / `gn_hwqc`）没有官方文档，只能从这两个列表里现取。
//!
//! 一行 13 个字段，实测（2026-09-18 fixture）依次是：
//! 代码, 名称, 成分数, 均价, 均涨跌额, **涨跌幅%**, 成交量, **成交额**, 领涨股代码,
//! 领涨股涨幅, 领涨股现价, 领涨股涨跌额, 领涨股名称。
//! 注意第 4 位是涨跌「额」、第 5 位才是涨跌「幅」—— 两者都像涨跌幅，取错了榜就是错的。
//!
//! 板块**成分股**走的是另一个端点（`Market_Center.getHQNodeData`），同一个 host，
//! 但格式完全不同：**UTF-8 JSON 数组**，中文是 `\uXXXX` 转义，不是 GBK。两条路都不需要 Referer。

use std::time::Duration;

use crate::core::sector::{Member, Sector, SectorKind, Snapshot};
use crate::core::symbol::Symbol;
use crate::source::throttle::Throttle;

#[derive(Debug, thiserror::Error)]
pub enum SectorError {
    #[error("请求失败：{0}")]
    Http(String),
    #[error("响应无法解析：{0}")]
    Parse(String),
    /// 新浪限流时返回的是空数组而不是错误码，必须当失败处理，否则退避永不触发。
    #[error("响应为空，多半是被限流了")]
    Empty,
}

const INDUSTRY_URL: &str = "http://vip.stock.finance.sina.com.cn/q/view/newSinaHy.php";
const CONCEPT_URL: &str = "http://money.finance.sina.com.cn/q/view/newFLJK.php?param=class";
const MEMBERS_URL: &str =
    "http://vip.stock.finance.sina.com.cn/quotes_service/api/json_v2.php/Market_Center.getHQNodeData";

/// 这个端点固定一页最多 100 条，要更多得翻页 —— 候选只取前 K 只，一页够了。
const MAX_PER_PAGE: usize = 100;

/// 板块列表一天只需要拉一次，间隔给得宽一点没有代价。
const MIN_INTERVAL: Duration = Duration::from_secs(5);

const I_NAME: usize = 1;
const I_CHANGE_PCT: usize = 5;
const I_TURNOVER: usize = 7;
const MIN_FIELDS: usize = 8;

pub struct SectorSource {
    http: reqwest::Client,
    throttle: Throttle,
}

impl SectorSource {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            // 新浪的接口在 HTTP/2 上会挂住不返回，全站强制 1.1
            http: reqwest::Client::builder()
                .http1_only()
                .timeout(Duration::from_secs(30))
                .user_agent("souba/0.1")
                .build()?,
            throttle: Throttle::new(MIN_INTERVAL),
        })
    }

    /// 这两个端点不需要 Referer（与日线接口不同）。
    pub async fn list(&self, kind: SectorKind) -> Result<Vec<Sector>, SectorError> {
        let url = match kind {
            SectorKind::Industry => INDUSTRY_URL,
            SectorKind::Concept => CONCEPT_URL,
        };
        self.throttle.acquire().await;
        let res = self.http.get(url).send().await.map_err(|e| {
            self.throttle.penalize();
            SectorError::Http(e.to_string())
        })?;
        if !res.status().is_success() {
            self.throttle.penalize();
            return Err(SectorError::Http(format!("HTTP {}", res.status())));
        }
        let body = res
            .bytes()
            .await
            .map_err(|e| SectorError::Http(e.to_string()))?;
        self.throttle.reset();
        parse_list(&body, kind)
    }

    /// 板块内按当日涨幅降序的前 `top` 只。与 `list` 共用同一个节流器 ——
    /// 同一个 host，分开节流等于没节流。
    pub async fn members(&self, sector_code: &str, top: usize) -> Result<Members, SectorError> {
        let url = format!(
            "{MEMBERS_URL}?page=1&num={}&sort=changepercent&asc=0&node={sector_code}&symbol=&_s_r_a=page",
            top.clamp(1, MAX_PER_PAGE)
        );
        self.throttle.acquire().await;
        let res = self.http.get(&url).send().await.map_err(|e| {
            self.throttle.penalize();
            SectorError::Http(e.to_string())
        })?;
        if !res.status().is_success() {
            self.throttle.penalize();
            return Err(SectorError::Http(format!("HTTP {}", res.status())));
        }
        let body = res
            .bytes()
            .await
            .map_err(|e| SectorError::Http(e.to_string()))?;
        let got = parse_members(&body)?;
        if got.list.is_empty() {
            self.throttle.penalize();
            return Err(SectorError::Empty);
        }
        self.throttle.reset();
        Ok(got)
    }
}

/// 一次成分股抓取的结果。`skipped` 是代码前缀不认识而丢掉的条数 ——
/// 静默丢掉会让「取了 20 只」变成 17 只却没人知道。
#[derive(Debug, Clone, PartialEq)]
pub struct Members {
    pub list: Vec<Member>,
    pub skipped: usize,
}

/// `sh688837` → `CN:688837`。新浪的成分股代码带交易所前缀，内部格式不带。
fn to_symbol(raw: &str) -> Option<Symbol> {
    let code = ["sh", "sz", "bj"].iter().find_map(|p| raw.strip_prefix(p))?;
    Symbol::parse(&format!("CN:{code}")).ok()
}

/// UTF-8 JSON，不是 GBK —— 跟同一个 host 上的板块列表端点不一样。
/// 响应已按涨幅降序，这里原样保序。
pub fn parse_members(json: &[u8]) -> Result<Members, SectorError> {
    #[derive(serde::Deserialize)]
    struct Row {
        symbol: String,
        name: String,
        changepercent: f64,
    }
    let rows: Vec<Row> =
        serde_json::from_slice(json).map_err(|e| SectorError::Parse(e.to_string()))?;
    let mut list = Vec::with_capacity(rows.len());
    let mut skipped = 0;
    for r in rows {
        match to_symbol(&r.symbol) {
            Some(symbol) => list.push(Member {
                symbol,
                name: r.name.trim().to_string(),
                change_pct: r.changepercent,
            }),
            None => skipped += 1,
        }
    }
    Ok(Members { list, skipped })
}

pub fn parse_list(gbk: &[u8], kind: SectorKind) -> Result<Vec<Sector>, SectorError> {
    let (text, _, _) = encoding_rs::GBK.decode(gbk);
    let start = text
        .find('{')
        .ok_or_else(|| SectorError::Parse("响应里没有 JSON 对象".into()))?;
    let map: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&text[start..]).map_err(|e| SectorError::Parse(e.to_string()))?;

    let mut out = Vec::with_capacity(map.len());
    for (code, row) in &map {
        let Some(row) = row.as_str() else { continue };
        let fields: Vec<&str> = row.split(',').collect();
        if fields.len() < MIN_FIELDS {
            return Err(SectorError::Parse(format!(
                "板块 {code} 只有 {} 个字段",
                fields.len()
            )));
        }
        out.push(Sector {
            code: code.clone(),
            name: fields[I_NAME].trim().to_string(),
            kind,
            snapshot: Snapshot {
                change_pct: num(fields[I_CHANGE_PCT], "涨跌幅", code)?,
                turnover: num(fields[I_TURNOVER], "成交额", code)?,
            },
        });
    }
    Ok(out)
}

fn num(raw: &str, field: &str, code: &str) -> Result<f64, SectorError> {
    raw.trim()
        .parse()
        .map_err(|_| SectorError::Parse(format!("板块 {code} 的{field} {raw:?} 不是数字")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDUSTRY: &[u8] = include_bytes!("../../tests/fixtures/sina_sector_industry.txt");
    const CONCEPT: &[u8] = include_bytes!("../../tests/fixtures/sina_sector_concept.txt");
    /// gn_hwqc（华为汽车）的前 25 条，2026-09-18 实抓
    const MEMBERS: &[u8] = include_bytes!("../../tests/fixtures/sina_sector_members.json");

    fn find<'a>(list: &'a [Sector], code: &str) -> &'a Sector {
        list.iter()
            .find(|s| s.code == code)
            .unwrap_or_else(|| panic!("fixture 里找不到 {code}"))
    }

    #[test]
    fn 行业列表解析出中文名与数值() {
        let list = parse_list(INDUSTRY, SectorKind::Industry).unwrap();
        assert_eq!(list.len(), 49);
        // fixture 原始行：new_blhy,玻璃行业,19,17.608947368421,-0.17052631578947,-0.95911903140819,893637084,23946459600,...
        let s = find(&list, "new_blhy");
        assert_eq!(s.name, "玻璃行业", "GBK 没解对");
        assert_eq!(s.kind, SectorKind::Industry);
        assert!((s.snapshot.change_pct - -0.95911903140819).abs() < 1e-9);
        assert!((s.snapshot.turnover - 23_946_459_600.0).abs() < 1.0);
    }

    #[test]
    fn 取的是涨跌幅而不是涨跌额() {
        // 第 4 位 -0.1705 是均涨跌额，第 5 位 -0.9591 才是涨跌幅。
        // 取错了整个热度榜都是错的，所以单独钉一条。
        let list = parse_list(INDUSTRY, SectorKind::Industry).unwrap();
        let s = find(&list, "new_blhy");
        assert!(
            (s.snapshot.change_pct - -0.17052631578947).abs() > 1e-6,
            "取成涨跌额了：{:?}",
            s.snapshot
        );
    }

    #[test]
    fn 概念列表解析且数量远多于行业() {
        let list = parse_list(CONCEPT, SectorKind::Concept).unwrap();
        assert_eq!(list.len(), 175);
        let s = find(&list, "gn_hwqc");
        assert_eq!(s.name, "华为汽车");
        assert_eq!(s.kind, SectorKind::Concept);
        assert!((s.snapshot.change_pct - 1.2030043900714).abs() < 1e-9);
        assert!((s.snapshot.turnover - 33_949_893_206.0).abs() < 1.0);
    }

    #[test]
    fn 每个板块都有非空名称与非负成交额() {
        for kind in [SectorKind::Industry, SectorKind::Concept] {
            let raw = match kind {
                SectorKind::Industry => INDUSTRY,
                SectorKind::Concept => CONCEPT,
            };
            for s in parse_list(raw, kind).unwrap() {
                assert!(!s.name.is_empty(), "{} 没有名称", s.code);
                assert!(!s.code.is_empty());
                assert!(s.snapshot.turnover >= 0.0, "{} 成交额为负", s.code);
                assert!(s.snapshot.change_pct.is_finite());
            }
        }
    }

    #[test]
    fn 损坏输入报错而不是panic() {
        assert!(parse_list(b"not js", SectorKind::Industry).is_err());
        assert!(parse_list(b"var x = {", SectorKind::Industry).is_err());
        assert!(
            parse_list(b"var x = {\"a\":\"a,b\"}", SectorKind::Industry).is_err(),
            "字段数不够应报错，不能当成 0 涨幅的板块混进榜单"
        );
    }

    #[test]
    fn 成分股解析出中文名与代码并保持涨幅降序() {
        let got = parse_members(MEMBERS).unwrap();
        assert_eq!(got.list.len(), 25);
        assert_eq!(got.skipped, 0);
        // fixture 首行：{"symbol":"sz002965","name":"\u7965\u946b\u79d1\u6280","changepercent":10.007,...}
        let first = &got.list[0];
        assert_eq!(first.symbol.to_string(), "CN:002965", "sz 前缀没剥掉");
        assert_eq!(first.name, "祥鑫科技", "\\uXXXX 没解对 —— 这个端点是 UTF-8 不是 GBK");
        assert!((first.change_pct - 10.007).abs() < 1e-9);
        let pcts: Vec<f64> = got.list.iter().map(|m| m.change_pct).collect();
        assert!(
            pcts.windows(2).all(|w| w[0] >= w[1]),
            "响应本身按涨幅降序，解析不该打乱：{pcts:?}"
        );
    }

    #[test]
    fn 三个交易所前缀都认_其余跳过并计数() {
        let raw = br#"[
          {"symbol":"sh688837","code":"688837","name":"a","changepercent":1.0},
          {"symbol":"sz300750","code":"300750","name":"b","changepercent":0.5},
          {"symbol":"bj920819","code":"920819","name":"c","changepercent":0.2},
          {"symbol":"hk00700","code":"00700","name":"d","changepercent":0.1},
          {"symbol":"","code":"","name":"e","changepercent":0.0}
        ]"#;
        let got = parse_members(raw).unwrap();
        let codes: Vec<String> = got.list.iter().map(|m| m.symbol.to_string()).collect();
        assert_eq!(codes, ["CN:688837", "CN:300750", "CN:920819"]);
        assert_eq!(got.skipped, 2, "不认识的代码要计数，不能静默吞掉");
    }

    #[test]
    fn 成分股损坏输入报错而不是panic() {
        assert!(parse_members(b"not json").is_err());
        assert!(parse_members(b"null").is_err(), "限流时返回的 null 不能当成空板块");
        // 空数组本身能解析，判空由 members() 转成 Empty 走退避
        assert_eq!(parse_members(b"[]").unwrap().list.len(), 0);
    }

    /// 联网冒烟：真拉一次两个端点。手动跑 `cargo test -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "联网"]
    async fn 联网拉取两个列表() {
        let src = SectorSource::new().unwrap();
        let hy = src.list(SectorKind::Industry).await.unwrap();
        let gn = src.list(SectorKind::Concept).await.unwrap();
        println!("行业 {} 个，概念 {} 个", hy.len(), gn.len());
        assert!(hy.len() >= 30, "行业只有 {} 个", hy.len());
        assert!(gn.len() >= 100, "概念只有 {} 个", gn.len());
        assert!(hy.iter().all(|s| !s.name.is_empty()));
        assert!(gn.iter().any(|s| s.snapshot.turnover > 0.0));
    }

    /// 联网冒烟：真实板块要能取到至少 K 只候选。
    /// `cargo test -- --ignored --nocapture 联网拉取板块成分股`
    #[tokio::test]
    #[ignore = "联网"]
    async fn 联网拉取板块成分股() {
        const K: usize = 20;
        let src = SectorSource::new().unwrap();
        let got = src.members("gn_hwqc", K).await.unwrap();
        println!("取到 {} 只，跳过 {}", got.list.len(), got.skipped);
        for m in got.list.iter().take(3) {
            println!("  {} {} {:+.2}%", m.symbol, m.name, m.change_pct);
        }
        assert!(got.list.len() >= K, "只取到 {} 只", got.list.len());
        assert!(got.list.iter().all(|m| !m.name.is_empty()));
        assert!(got.list.iter().all(|m| m.change_pct.is_finite()));
    }
}
