//! 与 Worker 的双向同步：启动时拉、扫描结束后推、`souba sync` 手动跑一次。
//!
//! 本地 SQLite 是运行时真相源，D1 是另一台机器的入口。合并规则很短：
//!
//! - `bars` / `adj_factors` / 板块系列 / 扫描结果只增不改 → 取并集，按主键 upsert 即可。
//! - `watchlist` / `settings` 两边整表交换，按 `updated_at` 后写者胜，**两边都不删行**。
//! - `ai_cache` 不参与同步 —— 丢了重问就是了，不值得占 D1 额度。
//!
//! Worker 不可达不是错误路径上的意外，是常态之一（免费额度会用完）：
//! 这里的任何失败都只落到顶栏的「未同步：<原因>」，绝不打断看盘。

use std::time::Instant;

use serde_json::{Value, json};

use crate::source::backfill;
use crate::store::{Row, Store};

/// 一张参与同步的表：表名、全部列、增量游标列。
/// 与 Worker `src/index.ts` 里的 `SECTOR_TABLES` / `FULL_TABLES` 一一对应，
/// 两边对不上就会出现「推上去 400、拉回来缺列」，所以改一边必须改另一边。
pub struct Spec {
    pub name: &'static str,
    pub cols: &'static [&'static str],
    pub cursor: &'static str,
    /// 游标列是 unix 秒（严格大于）；false = 业务日期（`>=`）
    pub strict: bool,
}

pub const BARS: Spec = Spec {
    name: "bars",
    cols: &[
        "symbol",
        "timeframe",
        "ts",
        "open",
        "high",
        "low",
        "close",
        "volume",
    ],
    cursor: "ts",
    strict: true,
};

pub const WATCHLIST: Spec = Spec {
    name: "watchlist",
    cols: &["symbol", "name", "sort_order", "added_at", "updated_at"],
    cursor: "updated_at",
    strict: true,
};

pub const SETTINGS: Spec = Spec {
    name: "settings",
    cols: &["key", "value", "updated_at"],
    cursor: "updated_at",
    strict: true,
};

/// `/sync/sectors` 一次交换的六张表
pub const SECTOR_TABLES: &[Spec] = &[
    Spec {
        name: "sectors",
        cols: &["market", "code", "name", "kind", "updated_at"],
        cursor: "updated_at",
        strict: true,
    },
    Spec {
        name: "sector_daily",
        cols: &["market", "sector_code", "date", "change_pct", "turnover"],
        cursor: "date",
        strict: false,
    },
    Spec {
        name: "sector_members",
        cols: &["market", "sector_code", "symbol", "name", "as_of", "rank"],
        cursor: "as_of",
        strict: false,
    },
    Spec {
        name: "scan_results",
        cols: &[
            "date",
            "market",
            "sector_code",
            "symbol",
            "rank",
            "stance",
            "freshness",
            "facets_json",
        ],
        cursor: "date",
        strict: false,
    },
    Spec {
        name: "adj_factors",
        cols: &["symbol", "effective_date", "factor"],
        cursor: "effective_date",
        strict: false,
    },
    Spec {
        name: "backfill_state",
        cols: &["symbol", "timeframe", "status", "updated_at"],
        cursor: "updated_at",
        strict: true,
    },
];

/// Worker 单次请求的行数上限（`MAX_POST_BARS`）
const CHUNK: usize = 2000;

/// 日期游标的下界。`sector_daily` 这些表用业务日期当游标，没有「0」这个值。
const EPOCH_DAY: &str = "1970-01-01";

fn spec(name: &str) -> &'static Spec {
    SECTOR_TABLES
        .iter()
        .find(|s| s.name == name)
        .expect("表名来自本模块的常量")
}

// ── 顶栏状态 ───────────────────────────────────────────────────────────────

/// 同步的当前状态。文案在 `ui::sync_label`。
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// 还没跑过（刚启动的那一瞬）
    Idle,
    Running,
    /// 成功，附最后一次成功的本地时刻 `HH:MM`
    Ok(String),
    /// 不可达 / 401 / 超时 / 5xx / 没配置。原因原样带给用户。
    Failed(String),
}

// ── 合并规则（纯函数）─────────────────────────────────────────────────────

fn updated_at(r: &Row) -> i64 {
    // 缺 updated_at 的行按「最旧」处理：宁可让对面的值赢，也不拿一行来路不明的
    // 数据盖掉明确带时间戳的那行
    r.get("updated_at").and_then(Value::as_i64).unwrap_or(i64::MIN)
}

fn key_of(r: &Row, key: &str) -> String {
    r.get(key).map(value_text).unwrap_or_default()
}

fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `watchlist` / `settings` 的整表合并：同一行按 `updated_at` 后写者胜，
/// 只有一边有的行两边都留下 —— **永远不删行**。
///
/// 返回 `(要写进本地的行, 合并后的整表)`。整表原样 PUT 给 Worker：
/// 它那边还会用 `updated_at >=` 再守一道，所以把没变的行一起送过去是安全的。
///
/// `updated_at` 相等时保留本地值：两边同一秒改了同一行没有客观胜者，
/// 挑一个确定的赢家，至少两台机器最终会收敛到同一份。
pub fn merge_rows(local: &[Row], remote: &[Row], key: &str) -> (Vec<Row>, Vec<Row>) {
    let mut to_local = Vec::new();
    let mut merged: Vec<(String, Row)> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for r in remote {
        let k = key_of(r, key);
        seen.insert(k.clone());
        match local.iter().find(|l| key_of(l, key) == k) {
            Some(l) if updated_at(l) >= updated_at(r) => merged.push((k, l.clone())),
            Some(_) | None => {
                to_local.push(r.clone());
                merged.push((k, r.clone()));
            }
        }
    }
    // 本地独有的行推给远端
    for l in local {
        let k = key_of(l, key);
        if !seen.contains(&k) {
            merged.push((k, l.clone()));
        }
    }
    merged.sort_by(|a, b| a.0.cmp(&b.0));
    (to_local, merged.into_iter().map(|(_, r)| r).collect())
}

/// 远端已经回补完、本地还没有的标的。
///
/// 这些标的的历史 bar 的 `ts` 远早于本机的 `pull:bars` 游标（另一台机器补的是
/// 2001 年起的 5990 根），按 `since` 增量拉永远看不到它们 ——
/// 必须带 `symbol=` 单独全量拉一次。
pub fn new_done(store: &Store, remote_backfill: &[Row]) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    for r in remote_backfill {
        if r.get("status").map(value_text).as_deref() != Some(backfill::DONE) {
            continue;
        }
        let symbol = key_of(r, "symbol");
        let tf = key_of(r, "timeframe");
        let local = store.sync_rows(
            "backfill_state",
            spec("backfill_state").cols,
            "updated_at",
            true,
            &json!(i64::MIN),
            &[("symbol", &symbol), ("timeframe", &tf)],
        )?;
        let done_here = local
            .first()
            .and_then(|l| l.get("status"))
            .map(value_text)
            .as_deref()
            == Some(backfill::DONE);
        if !done_here && !out.contains(&symbol) {
            out.push(symbol);
        }
    }
    Ok(out)
}

// ── 推送计划（游标）────────────────────────────────────────────────────────

/// 一个推送单元：一次把 `rows` 全部推成功了，才把 `cursor` 记到 `sync_state[key]`。
///
/// 单元内部还会按 2000 行切片发请求，但游标只在整个单元推完后前进 ——
/// 中途失败下次从这个单元的开头重推，upsert 幂等，重推无害。
#[derive(Debug, Clone, PartialEq)]
pub struct Unit {
    pub table: &'static str,
    pub key: String,
    pub cursor: String,
    pub rows: Vec<Row>,
}

impl Unit {
    pub fn chunks(&self) -> impl Iterator<Item = &[Row]> {
        self.rows.chunks(CHUNK)
    }
}

fn cursor_since(store: &Store, key: &str, strict: bool) -> anyhow::Result<Value> {
    let raw = store.sync_state_get(key)?;
    Ok(match (raw, strict) {
        // 没推过：整型游标用 i64::MIN 而不是 0 —— 0 会漏掉 1970 年以前的 ts，
        // 也会漏掉 updated_at 为 0 的行
        (None, true) => json!(i64::MIN),
        (None, false) => json!(EPOCH_DAY),
        (Some(v), true) => json!(v.parse::<i64>().unwrap_or(i64::MIN)),
        (Some(v), false) => json!(v),
    })
}

fn max_cursor(rows: &[Row], cursor: &str) -> Option<String> {
    rows.iter()
        .map(|r| r.get(cursor).map(value_text).unwrap_or_default())
        .max_by(|a, b| match (a.parse::<i64>(), b.parse::<i64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            _ => a.cmp(b),
        })
}

/// 要推的 bar：只推 `backfill_state` 已 `done` 的标的。
///
/// 补了一半的历史推上去，另一台机器会拿它当完整历史去算 EMA576 ——
/// 那是把「数据不足」偷偷变成一条不可信的线，正是这个项目不许犯的错。
///
/// 游标按 (标的, 周期) 各记一份，而不是全局一个 `ts`：新标的回补的是 2001 年起的
/// 老 bar，全局游标早就越过那个时间了，它们会一根都推不出去。
pub fn bars_to_push(store: &Store) -> anyhow::Result<Vec<Unit>> {
    let bf = spec("backfill_state");
    let states = store.sync_rows(bf.name, bf.cols, bf.cursor, true, &json!(i64::MIN), &[])?;
    let mut out = Vec::new();
    for st in states {
        if st.get("status").map(value_text).as_deref() != Some(backfill::DONE) {
            continue;
        }
        let symbol = key_of(&st, "symbol");
        let tf = key_of(&st, "timeframe");
        let key = format!("push:bars:{symbol}:{tf}");
        let since = cursor_since(store, &key, true)?;
        let rows = store.sync_rows(
            BARS.name,
            BARS.cols,
            BARS.cursor,
            true,
            &since,
            &[("symbol", &symbol), ("timeframe", &tf)],
        )?;
        if let Some(cursor) = max_cursor(&rows, BARS.cursor) {
            out.push(Unit {
                table: BARS.name,
                key,
                cursor,
                rows,
            });
        }
    }
    Ok(out)
}

/// 要推的板块系列 / 因子 / 扫描结果，每张表按自己的游标增量取。
pub fn sectors_to_push(store: &Store) -> anyhow::Result<Vec<Unit>> {
    let mut out = Vec::new();
    for s in SECTOR_TABLES {
        let key = format!("push:{}", s.name);
        let since = cursor_since(store, &key, s.strict)?;
        let rows = store.sync_rows(s.name, s.cols, s.cursor, s.strict, &since, &[])?;
        if let Some(cursor) = max_cursor(&rows, s.cursor) {
            out.push(Unit {
                table: s.name,
                key,
                cursor,
                rows,
            });
        }
    }
    Ok(out)
}

// ── HTTP ───────────────────────────────────────────────────────────────────

/// `.env` 里的两个键。地址与密钥缺一个就等于没配同步。
const URL_KEY: &str = "SOUBA_SYNC_URL";
const KEY_KEY: &str = "SOUBA_SYNC_KEY";

pub struct Client {
    http: reqwest::Client,
    base: String,
    key: String,
}

impl Client {
    /// 没配 `SOUBA_SYNC_URL` / `SOUBA_SYNC_KEY` 时返回 `Err`，文案直接进顶栏 ——
    /// 「同步没在跑」必须说得出为什么，静默地不同步才是最坏的那种。
    pub fn from_env() -> anyhow::Result<Self> {
        let vars = crate::dotenv::load(&[URL_KEY, KEY_KEY]);
        let base = vars
            .get(URL_KEY)
            .ok_or_else(|| anyhow::anyhow!("未配置 {URL_KEY}"))?
            .trim_end_matches('/')
            .to_string();
        let key = vars
            .get(KEY_KEY)
            .ok_or_else(|| anyhow::anyhow!("未配置 {KEY_KEY}"))?
            .clone();
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .user_agent("souba/0.1")
                .build()?,
            base,
            key,
        })
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> anyhow::Result<Value> {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.base))
            .header("X-Souba-Key", &self.key);
        // reqwest 没开 json 特性，手动序列化 + 设 Content-Type（与 `ai::post` 一致）
        if let Some(b) = body {
            req = req
                .header("Content-Type", "application/json")
                .body(serde_json::to_string(&b)?);
        }
        let res = req
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("连不上 Worker：{}", short(&e.to_string())))?;
        let status = res.status();
        let text = res
            .text()
            .await
            .map_err(|e| anyhow::anyhow!("读不到 Worker 的响应：{}", short(&e.to_string())))?;
        if status == reqwest::StatusCode::UNAUTHORIZED {
            anyhow::bail!("密钥不匹配（401）");
        }
        if !status.is_success() {
            anyhow::bail!("Worker 返回 {}：{}", status.as_u16(), short(&text));
        }
        serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("Worker 的响应不是合法 JSON：{e}"))
    }
}

/// 顶栏只有一行，原因截到能看清又不撑破的长度
fn short(s: &str) -> String {
    let t = s.trim().replace('\n', " ");
    match t.char_indices().nth(80) {
        Some((i, _)) => format!("{}…", &t[..i]),
        None => t,
    }
}

fn rows_of(v: &Value, table: &str) -> Vec<Row> {
    v.get(table)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|r| r.as_object().cloned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

// ── 拉 ─────────────────────────────────────────────────────────────────────

/// 分页拉 bar 并落库，返回落库行数。`symbol` 为 `Some` 时按标的全量拉。
async fn pull_bars(
    store: &Store,
    c: &Client,
    symbol: Option<&str>,
    state_key: Option<&str>,
) -> anyhow::Result<usize> {
    let mut since: i64 = match state_key {
        Some(k) => store
            .sync_state_get(k)?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        None => 0,
    };
    let mut total = 0;
    // 5990 根一只，一页 5000 —— bars 这条路必须翻页，翻不动才停
    loop {
        let q = match symbol {
            Some(s) => format!("/sync/bars?since={since}&symbol={s}"),
            None => format!("/sync/bars?since={since}"),
        };
        let body = c.call(reqwest::Method::GET, &q, None).await?;
        let rows = rows_of(&body, "bars");
        if rows.is_empty() {
            break;
        }
        total += store.sync_replace(BARS.name, BARS.cols, &rows)?;
        let next = body.get("next").and_then(Value::as_i64);
        match next {
            Some(n) if n > since => since = n,
            // next 没往前走就说明这一页全是同一个 ts，再拉一次还是它 —— 停下来
            _ => break,
        }
        if let Some(k) = state_key {
            store.sync_state_set(k, &since.to_string())?;
        }
    }
    if let Some(k) = state_key {
        store.sync_state_set(k, &since.to_string())?;
    }
    Ok(total)
}

/// 板块系列的一次增量拉。返回 (落库行数, 这次拿到的 backfill_state 行)。
async fn pull_sectors(
    store: &Store,
    c: &Client,
    since: i64,
    since_date: &str,
    write_only: Option<&str>,
) -> anyhow::Result<(usize, Vec<Row>, i64, String)> {
    let body = c
        .call(
            reqwest::Method::GET,
            &format!("/sync/sectors?since={since}&since_date={since_date}"),
            None,
        )
        .await?;
    // 先把远端的 backfill_state 拿出来跟本地比 —— 写进去之后就比不出来了
    let remote_bf = rows_of(&body, "backfill_state");
    let mut total = 0;
    for s in SECTOR_TABLES {
        if write_only.is_some_and(|only| only != s.name) {
            continue;
        }
        total += store.sync_replace(s.name, s.cols, &rows_of(&body, s.name))?;
    }
    let next_since = body
        .pointer("/next/since")
        .and_then(Value::as_i64)
        .unwrap_or(since);
    let next_date = body
        .pointer("/next/since_date")
        .and_then(Value::as_str)
        .unwrap_or(since_date)
        .to_string();
    Ok((total, remote_bf, next_since, next_date))
}

/// `watchlist` / `settings` 的整表交换：拉远端 → 合并 → 本地写胜者 → 整表 PUT 回去。
async fn exchange(store: &Store, c: &Client, s: &Spec, key: &str) -> anyhow::Result<(usize, usize)> {
    let path = format!("/sync/{}", s.name);
    let body = c.call(reqwest::Method::GET, &path, None).await?;
    let remote = rows_of(&body, s.name);
    let local = store.sync_rows(s.name, s.cols, s.cursor, true, &json!(i64::MIN), &[])?;
    let (to_local, merged) = merge_rows(&local, &remote, key);
    let pulled = store.sync_replace(s.name, s.cols, &to_local)?;
    let mut pushed = 0;
    for chunk in merged.chunks(CHUNK) {
        c.call(reqwest::Method::PUT, &path, Some(json!(chunk)))
            .await?;
        pushed += chunk.len();
    }
    Ok((pulled, pushed))
}

pub async fn pull(store: &Store, c: &Client) -> anyhow::Result<usize> {
    let mut total = pull_bars(store, c, None, Some("pull:bars")).await?;

    let since: i64 = store
        .sync_state_get("pull:sectors:since")?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let since_date = store
        .sync_state_get("pull:sectors:date")?
        .unwrap_or_else(|| EPOCH_DAY.to_string());
    let (n, remote_bf, next_since, next_date) =
        pull_sectors(store, c, since, &since_date, None).await?;
    total += n;

    // 另一台机器新补完的标的：它的老 bar 在本机游标之前，只能按标的单独全量拉。
    // 因子同理 —— adj_factors 的游标是除权日，新标的的除权日全在过去。
    let fresh = new_done(store, &remote_bf)?;
    if !fresh.is_empty() {
        // ponytail: 一次请求封顶 5000 行/表，因子几百行够用；顶到了就得翻页
        let (n, ..) = pull_sectors(store, c, 0, EPOCH_DAY, Some("adj_factors")).await?;
        total += n;
        for symbol in &fresh {
            total += pull_bars(store, c, Some(symbol), None).await?;
        }
    }
    store.sync_state_set("pull:sectors:since", &next_since.to_string())?;
    store.sync_state_set("pull:sectors:date", &next_date)?;

    let (a, _) = exchange(store, c, &WATCHLIST, "symbol").await?;
    let (b, _) = exchange(store, c, &SETTINGS, "key").await?;
    Ok(total + a + b)
}

// ── 推 ─────────────────────────────────────────────────────────────────────

pub async fn push(store: &Store, c: &Client) -> anyhow::Result<usize> {
    let mut units = bars_to_push(store)?;
    units.extend(sectors_to_push(store)?);
    let mut total = 0;
    for u in units {
        for chunk in u.chunks() {
            let body = if u.table == BARS.name {
                json!({ "bars": chunk })
            } else {
                json!({ u.table: chunk })
            };
            let path = if u.table == BARS.name {
                "/sync/bars"
            } else {
                "/sync/sectors"
            };
            // 一片失败就停下：这个单元的游标不前进，之前推完的单元的游标留着，
            // 下次从这个单元的开头接着推
            c.call(reqwest::Method::POST, path, Some(body)).await?;
            total += chunk.len();
        }
        store.sync_state_set(&u.key, &u.cursor)?;
    }
    Ok(total)
}

// ── 编排 ───────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    pub pulled: usize,
    pub pushed: usize,
}

/// 先拉后推。`do_pull = false` 时只推（扫描结束后那一次）。
pub async fn run(store: &Store, c: &Client, do_pull: bool) -> anyhow::Result<Report> {
    let pulled = if do_pull { pull(store, c).await? } else { 0 };
    let pushed = push(store, c).await?;
    store.sync_state_set("last_ok", &chrono::Utc::now().timestamp().to_string())?;
    Ok(Report { pulled, pushed })
}

/// 后台跑一次，结果只落成顶栏状态。永远返回 `Status`，绝不打断看盘。
pub async fn background(store: &Store, do_pull: bool) -> Status {
    let c = match Client::from_env() {
        Ok(c) => c,
        Err(e) => return Status::Failed(short(&e.to_string())),
    };
    match run(store, &c, do_pull).await {
        Ok(_) => Status::Ok(chrono::Local::now().format("%H:%M").to_string()),
        Err(e) => Status::Failed(short(&e.to_string())),
    }
}

/// `souba sync`：同步执行并打印摘要。
pub async fn run_cli(store: &Store) -> anyhow::Result<()> {
    let started = Instant::now();
    let c = Client::from_env()?;
    let r = run(store, &c, true).await?;
    println!(
        "同步完成：拉取 {} 行，推送 {} 行，耗时 {:.1} 秒",
        r.pulled,
        r.pushed,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// 冒烟测试用的假标的，见下方 `#[ignore]` 测试的说明
#[cfg(test)]
const SMOKE_SYMBOL: &str = "CN:999999";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::bar::{Bar, Timeframe};
    use crate::core::symbol::Symbol;

    fn row(pairs: &[(&str, Value)]) -> Row {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    fn watch_row(symbol: &str, name: &str, updated_at: i64) -> Row {
        row(&[
            ("symbol", json!(symbol)),
            ("name", json!(name)),
            ("sort_order", json!(0)),
            ("added_at", json!(1)),
            ("updated_at", json!(updated_at)),
        ])
    }

    fn sym(s: &str) -> Symbol {
        Symbol::parse(s).unwrap()
    }

    fn bar(ts: i64) -> Bar {
        Bar {
            ts: chrono::DateTime::from_timestamp(ts, 0).unwrap(),
            open: 1.0,
            high: 2.0,
            low: 0.5,
            close: 1.5,
            volume: 100.0,
        }
    }

    // ── 合并规则

    #[test]
    fn 两边各自新增的行都留下来() {
        let local = [watch_row("CN:600519", "贵州茅台", 10)];
        let remote = [watch_row("HK:00700", "腾讯控股", 20)];
        let (to_local, merged) = merge_rows(&local, &remote, "symbol");
        assert_eq!(to_local.len(), 1);
        assert_eq!(to_local[0]["symbol"], json!("HK:00700"));
        let keys: Vec<&str> = merged.iter().map(|r| r["symbol"].as_str().unwrap()).collect();
        assert_eq!(keys, vec!["CN:600519", "HK:00700"]);
    }

    #[test]
    fn 同一行远端更新则远端赢() {
        let local = [watch_row("CN:600519", "旧名", 10)];
        let remote = [watch_row("CN:600519", "新名", 20)];
        let (to_local, merged) = merge_rows(&local, &remote, "symbol");
        assert_eq!(to_local.len(), 1);
        assert_eq!(to_local[0]["name"], json!("新名"));
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0]["name"], json!("新名"));
    }

    #[test]
    fn 同一行本地更新则不写本地但整表带上本地值() {
        let local = [watch_row("CN:600519", "新名", 30)];
        let remote = [watch_row("CN:600519", "旧名", 20)];
        let (to_local, merged) = merge_rows(&local, &remote, "symbol");
        assert!(to_local.is_empty());
        assert_eq!(merged[0]["name"], json!("新名"));
    }

    #[test]
    fn 时间戳相等时保留本地() {
        let local = [watch_row("CN:600519", "本地", 20)];
        let remote = [watch_row("CN:600519", "远端", 20)];
        let (to_local, merged) = merge_rows(&local, &remote, "symbol");
        assert!(to_local.is_empty());
        assert_eq!(merged[0]["name"], json!("本地"));
    }

    #[test]
    fn 合并不删任何一边的行() {
        let local = [watch_row("A", "a", 1), watch_row("B", "b", 1)];
        let remote = [watch_row("B", "b2", 9), watch_row("C", "c", 1)];
        let (_, merged) = merge_rows(&local, &remote, "symbol");
        let keys: Vec<&str> = merged.iter().map(|r| r["symbol"].as_str().unwrap()).collect();
        assert_eq!(keys, vec!["A", "B", "C"]);
    }

    #[test]
    fn settings_按键名合并() {
        let local = [row(&[
            ("key", json!("vegas.fast")),
            ("value", json!("144")),
            ("updated_at", json!(5)),
        ])];
        let remote = [row(&[
            ("key", json!("vegas.fast")),
            ("value", json!("169")),
            ("updated_at", json!(9)),
        ])];
        let (to_local, _) = merge_rows(&local, &remote, "key");
        assert_eq!(to_local[0]["value"], json!("169"));
    }

    // ── 游标

    #[test]
    fn 已回补的标的第一次推全量第二次不再推() {
        let st = Store::open_in_memory().unwrap();
        let s = sym("CN:600519");
        st.upsert_bars(&s, Timeframe::Day, &[bar(100), bar(200)])
            .unwrap();
        st.set_backfill_status(&s, Timeframe::Day, backfill::DONE)
            .unwrap();

        let units = bars_to_push(&st).unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].rows.len(), 2);
        assert_eq!(units[0].cursor, "200");

        // 推成功后记游标
        st.sync_state_set(&units[0].key, &units[0].cursor).unwrap();
        assert!(bars_to_push(&st).unwrap().is_empty());

        // 新来一根只推这一根
        st.upsert_bars(&s, Timeframe::Day, &[bar(300)]).unwrap();
        let units = bars_to_push(&st).unwrap();
        assert_eq!(units[0].rows.len(), 1);
        assert_eq!(units[0].rows[0]["ts"], json!(300));
    }

    #[test]
    fn 没回补完的标的一根都不推() {
        let st = Store::open_in_memory().unwrap();
        let s = sym("CN:600519");
        st.upsert_bars(&s, Timeframe::Day, &[bar(100)]).unwrap();
        st.set_backfill_status(&s, Timeframe::Day, backfill::RUNNING)
            .unwrap();
        assert!(bars_to_push(&st).unwrap().is_empty());
    }

    #[test]
    fn 游标按标的分开记新标的的老历史照样推得出去() {
        let st = Store::open_in_memory().unwrap();
        let old = sym("CN:600519");
        st.upsert_bars(&old, Timeframe::Day, &[bar(9_000)]).unwrap();
        st.set_backfill_status(&old, Timeframe::Day, backfill::DONE)
            .unwrap();
        for u in bars_to_push(&st).unwrap() {
            st.sync_state_set(&u.key, &u.cursor).unwrap();
        }

        // 后来的标的补的是更早的历史 —— 全局游标会把它整只吞掉
        let fresh = sym("CN:000001");
        st.upsert_bars(&fresh, Timeframe::Day, &[bar(100), bar(200)])
            .unwrap();
        st.set_backfill_status(&fresh, Timeframe::Day, backfill::DONE)
            .unwrap();
        let units = bars_to_push(&st).unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].rows.len(), 2);
    }

    #[test]
    fn 板块系列第二次只推增量() {
        let st = Store::open_in_memory().unwrap();
        st.exec(
            "INSERT INTO sector_daily (market, sector_code, date, change_pct, turnover)
             VALUES ('CN','new_hy1','2026-09-17', 1.5, 100.0)",
        )
        .unwrap();
        let units = sectors_to_push(&st).unwrap();
        let daily = units.iter().find(|u| u.table == "sector_daily").unwrap();
        assert_eq!(daily.rows.len(), 1);
        assert_eq!(daily.cursor, "2026-09-17");
        st.sync_state_set(&daily.key, &daily.cursor).unwrap();

        // date 游标是 >=，同一天的行会再来一次（幂等，故意的）；换一天只来新的
        st.exec(
            "INSERT INTO sector_daily (market, sector_code, date, change_pct, turnover)
             VALUES ('CN','new_hy2','2026-09-18', 2.5, 200.0)",
        )
        .unwrap();
        let units = sectors_to_push(&st).unwrap();
        let daily = units.iter().find(|u| u.table == "sector_daily").unwrap();
        let dates: Vec<&str> = daily
            .rows
            .iter()
            .map(|r| r["date"].as_str().unwrap())
            .collect();
        assert_eq!(dates, vec!["2026-09-17", "2026-09-18"]);
    }

    #[test]
    fn 空表不产生推送单元() {
        let st = Store::open_in_memory().unwrap();
        assert!(sectors_to_push(&st).unwrap().is_empty());
        assert!(bars_to_push(&st).unwrap().is_empty());
    }

    // ── 新标的的历史

    #[test]
    fn 远端已回补而本地没有的标的要单独全量拉() {
        let st = Store::open_in_memory().unwrap();
        st.set_backfill_status(&sym("CN:600519"), Timeframe::Day, backfill::DONE)
            .unwrap();
        st.set_backfill_status(&sym("CN:000002"), Timeframe::Day, backfill::RUNNING)
            .unwrap();
        let remote = [
            // 本地也 done —— 不用管
            row(&[
                ("symbol", json!("CN:600519")),
                ("timeframe", json!("1d")),
                ("status", json!("done")),
                ("updated_at", json!(1)),
            ]),
            // 本地停在 running —— 要拉
            row(&[
                ("symbol", json!("CN:000002")),
                ("timeframe", json!("1d")),
                ("status", json!("done")),
                ("updated_at", json!(1)),
            ]),
            // 本地没有这行 —— 要拉
            row(&[
                ("symbol", json!("CN:000001")),
                ("timeframe", json!("1d")),
                ("status", json!("done")),
                ("updated_at", json!(1)),
            ]),
            // 远端自己也没补完 —— 不拉
            row(&[
                ("symbol", json!("CN:000003")),
                ("timeframe", json!("1d")),
                ("status", json!("running")),
                ("updated_at", json!(1)),
            ]),
        ];
        let mut got = new_done(&st, &remote).unwrap();
        got.sort();
        assert_eq!(got, vec!["CN:000001", "CN:000002"]);
    }

    // ── 通用行读写

    #[test]
    fn 远端回来的行能原样落库再读回() {
        let st = Store::open_in_memory().unwrap();
        let rows = vec![row(&[
            ("symbol", json!("CN:600519")),
            ("timeframe", json!("1d")),
            ("ts", json!(1_000)),
            ("open", json!(10.5)),
            ("high", json!(11.0)),
            ("low", json!(10.0)),
            ("close", json!(10.8)),
            ("volume", json!(123.0)),
        ])];
        assert_eq!(st.sync_replace(BARS.name, BARS.cols, &rows).unwrap(), 1);
        let back = st.raw_bars(&sym("CN:600519"), Timeframe::Day).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].close, 10.8);
        // 幂等：同一主键再写一次不会多出一行
        st.sync_replace(BARS.name, BARS.cols, &rows).unwrap();
        assert_eq!(st.raw_bars(&sym("CN:600519"), Timeframe::Day).unwrap().len(), 1);
    }

    #[test]
    fn 可空列也能落库() {
        let st = Store::open_in_memory().unwrap();
        let s = spec("scan_results");
        let rows = vec![row(&[
            ("date", json!("2026-09-18")),
            ("market", json!("CN")),
            ("sector_code", json!("hy1")),
            ("symbol", json!("CN:600519")),
            ("rank", json!(1)),
            ("stance", json!("Watch")),
            ("freshness", Value::Null),
            ("facets_json", json!("[]")),
        ])];
        assert_eq!(st.sync_replace(s.name, s.cols, &rows).unwrap(), 1);
        let back = st
            .sync_rows(s.name, s.cols, s.cursor, false, &json!(EPOCH_DAY), &[])
            .unwrap();
        assert_eq!(back[0]["freshness"], Value::Null);
    }

    #[test]
    fn 缺列的行整批拒绝() {
        let st = Store::open_in_memory().unwrap();
        let rows = vec![row(&[("symbol", json!("CN:600519"))])];
        assert!(st.sync_replace(BARS.name, BARS.cols, &rows).is_err());
        assert!(st.raw_bars(&sym("CN:600519"), Timeframe::Day).unwrap().is_empty());
    }

    #[test]
    fn 原因过长时截断() {
        let long = "错".repeat(200);
        let s = short(&long);
        assert!(s.chars().count() <= 81, "{s}");
        assert!(s.ends_with('…'));
    }

    // ── 联网冒烟

    /// 对真实 Worker 推一小批再拉回来，断言内容一致。
    ///
    /// 用的是假标的 `CN:999999`，因为 Worker 没有删接口 —— 这三根 bar 会**永久留在
    /// D1 里**。这是刻意的取舍：一个不存在的代码不会进任何扫描与自选，
    /// 留着三行垃圾比给同步协议开一个删接口便宜得多。
    #[tokio::test]
    #[ignore = "需要外网与 .env 里的 SOUBA_SYNC_URL / SOUBA_SYNC_KEY"]
    async fn 冒烟_推上去再拉回来内容一致() {
        let c = Client::from_env().expect("需要 .env 里的 SOUBA_SYNC_URL / SOUBA_SYNC_KEY");
        let st = Store::open_in_memory().unwrap();
        let s = sym(SMOKE_SYMBOL);
        let bars = [bar(1_600_000_000), bar(1_600_086_400), bar(1_600_172_800)];
        st.upsert_bars(&s, Timeframe::Day, &bars).unwrap();
        st.set_backfill_status(&s, Timeframe::Day, backfill::DONE)
            .unwrap();

        let pushed = push(&st, &c).await.expect("推送失败");
        assert!(pushed >= bars.len());

        // 换一个空库按标的全量拉回来
        let back = Store::open_in_memory().unwrap();
        let n = pull_bars(&back, &c, Some(SMOKE_SYMBOL), None)
            .await
            .expect("拉取失败");
        // 只断言「至少这几根」：`symbol=` 还没部署上去的 Worker 会忽略这个参数，
        // 把整个库都回给我们。这一只的内容对不对才是这个冒烟要证的事。
        assert!(n >= bars.len(), "拉回来的行数不对：{n}");
        let got = back.raw_bars(&s, Timeframe::Day).unwrap();
        assert_eq!(got.len(), bars.len(), "这只标的的根数对不上");
        for (a, b) in got.iter().zip(bars.iter()) {
            assert_eq!(a.ts, b.ts);
            assert_eq!(a.close, b.close);
        }
    }
}
