use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension};

use crate::core::sector::{Sector, Snapshot};
use crate::core::symbol::{Market, Symbol};
use crate::core::adjust::{AdjFactor, apply_factors};
use crate::core::bar::{Bar, Timeframe};

#[derive(Debug, Clone)]
pub struct WatchItem {
    pub symbol: Symbol,
    /// 库里缓存的名称。当前渲染用的是行情响应里的实时名称，
    /// 这个字段是为了离线时（拉不到行情）仍能显示名字 —— 阶段 2 接。
    #[allow(dead_code)]
    pub name: String,
}

pub struct Store {
    conn: Mutex<Connection>,
}

/// 与 Cloudflare D1 共用的迁移。一个元素一个版本，按序执行，
/// 执行到第 n 个就把 `PRAGMA user_version` 记成 n —— D1 没有 user_version，
/// 它用 `wrangler d1 migrations` 自己记账，所以 SQL 文件里不写 PRAGMA。
/// 直接 include 同一批文件，避免本地与 D1 的 schema 漂移。
const MIGRATIONS: &[&str] = &[
    include_str!("../../worker/migrations/0001_initial.sql"),
    include_str!("../../worker/migrations/0002_ashare_loop.sql"),
];

fn migrate(conn: &Connection) -> anyhow::Result<()> {
    let current: usize = conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? as usize;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
        conn.execute_batch(&format!(
            "BEGIN; {sql}\nPRAGMA user_version = {}; COMMIT;",
            i + 1
        ))?;
    }
    Ok(())
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        Self::from_conn(Connection::open(path)?)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> anyhow::Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> anyhow::Result<Self> {
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn default_path() -> anyhow::Result<PathBuf> {
        let dirs = directories::ProjectDirs::from("", "", "souba")
            .ok_or_else(|| anyhow::anyhow!("无法确定用户数据目录"))?;
        Ok(dirs.data_dir().join("souba.db"))
    }

    pub fn watchlist(&self) -> anyhow::Result<Vec<WatchItem>> {
        let conn = self.conn.lock().expect("store 锁中毒");
        let mut stmt = conn.prepare("SELECT symbol, name FROM watchlist ORDER BY sort_order")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (raw, name) = row?;
            match Symbol::parse(&raw) {
                Ok(symbol) => out.push(WatchItem { symbol, name }),
                // 库里出现无法解析的代码时跳过而不是整体失败 ——
                // 一条坏记录不该让用户打不开自选股列表
                Err(e) => eprintln!("[souba] 跳过无法解析的自选股 {raw:?}：{e}"),
            }
        }
        Ok(out)
    }

    /// 幂等：重复加入不产生重复项，并顺便更新名称（股票会改名）
    pub fn add(&self, symbol: &Symbol, name: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().expect("store 锁中毒");
        let next: i64 = conn.query_row(
            "SELECT COALESCE(MAX(sort_order), -1) + 1 FROM watchlist",
            [],
            |r| r.get(0),
        )?;
        conn.execute(
            "INSERT INTO watchlist (symbol, name, sort_order, added_at)
             VALUES (?1, ?2, ?3, unixepoch())
             ON CONFLICT(symbol) DO UPDATE SET name = excluded.name",
            rusqlite::params![symbol.to_string(), name, next],
        )?;
        Ok(())
    }

    pub fn all_settings(&self) -> anyhow::Result<Vec<(String, String)>> {
        let conn = self.conn.lock().expect("store 锁中毒");
        let mut stmt = conn.prepare("SELECT key, value FROM settings")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 覆盖写并刷新 updated_at —— D1 同步按它「最后写入者胜」
    pub fn set_setting(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().expect("store 锁中毒");
        conn.execute(
            "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, unixepoch())
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    /// 落一次板块列表与当日快照。
    ///
    /// 两张表一起写：`sectors` 是板块本身（改名了就更新），`sector_daily` 是当天的
    /// 涨幅与成交额。都按主键幂等 —— 同一天重跑扫描只会覆盖当天那行。
    pub fn record_sectors(
        &self,
        market: Market,
        date: &str,
        sectors: &[Sector],
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().expect("store 锁中毒");
        let tx = conn.unchecked_transaction()?;
        for s in sectors {
            tx.execute(
                "INSERT INTO sectors (market, code, name, kind, updated_at)
                 VALUES (?1, ?2, ?3, ?4, unixepoch())
                 ON CONFLICT(market, code) DO UPDATE SET
                   name = excluded.name, kind = excluded.kind, updated_at = excluded.updated_at",
                rusqlite::params![market.as_str(), s.code, s.name, s.kind.as_str()],
            )?;
            tx.execute(
                "INSERT INTO sector_daily (market, sector_code, date, change_pct, turnover)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(market, sector_code, date) DO UPDATE SET
                   change_pct = excluded.change_pct, turnover = excluded.turnover",
                rusqlite::params![
                    market.as_str(),
                    s.code,
                    date,
                    s.snapshot.change_pct,
                    s.snapshot.turnover
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// `date` 之前最近 `days` 天的快照，用来算热度。不足就返回实际有的。
    pub fn sector_history(
        &self,
        market: Market,
        code: &str,
        date: &str,
        days: usize,
    ) -> anyhow::Result<Vec<Snapshot>> {
        let conn = self.conn.lock().expect("store 锁中毒");
        let mut stmt = conn.prepare(
            "SELECT change_pct, turnover FROM sector_daily
             WHERE market = ?1 AND sector_code = ?2 AND date < ?3
             ORDER BY date DESC LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![market.as_str(), code, date, days as i64],
            |r| {
                Ok(Snapshot {
                    change_pct: r.get(0)?,
                    turnover: r.get(1)?,
                })
            },
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 批量写入**原始价** bar。幂等：主键冲突时覆盖，重复导入不会产生重复行。
    /// 覆盖而不是忽略，是因为源偶尔会修正当日的最后一根。
    pub fn upsert_bars(&self, symbol: &Symbol, tf: Timeframe, bars: &[Bar]) -> anyhow::Result<()> {
        let mut conn = self.conn.lock().expect("store 锁中毒");
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO bars (symbol, timeframe, ts, open, high, low, close, volume)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(symbol, timeframe, ts) DO UPDATE SET
                   open = excluded.open, high = excluded.high, low = excluded.low,
                   close = excluded.close, volume = excluded.volume",
            )?;
            let sym = symbol.to_string();
            for b in bars {
                stmt.execute(rusqlite::params![
                    sym,
                    tf.key(),
                    b.ts.timestamp(),
                    b.open,
                    b.high,
                    b.low,
                    b.close,
                    b.volume
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 库里存的原始价，不做任何复权。测试与同步用它；画图请用 `adjusted_bars`。
    pub fn raw_bars(&self, symbol: &Symbol, tf: Timeframe) -> anyhow::Result<Vec<Bar>> {
        let conn = self.conn.lock().expect("store 锁中毒");
        let mut stmt = conn.prepare(
            "SELECT ts, open, high, low, close, volume FROM bars
             WHERE symbol = ?1 AND timeframe = ?2 ORDER BY ts",
        )?;
        let rows = stmt.query_map(rusqlite::params![symbol.to_string(), tf.key()], |r| {
            Ok(Bar {
                ts: chrono::DateTime::from_timestamp(r.get::<_, i64>(0)?, 0)
                    .unwrap_or_default(),
                open: r.get(1)?,
                high: r.get(2)?,
                low: r.get(3)?,
                close: r.get(4)?,
                volume: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// 读时复权：原始价按因子表折算。因子变了结果就变，`bars` 一行不动。
    pub fn adjusted_bars(&self, symbol: &Symbol, tf: Timeframe) -> anyhow::Result<Vec<Bar>> {
        let mut bars = self.raw_bars(symbol, tf)?;
        let factors = self.adj_factors(symbol)?;
        apply_factors(&mut bars, &factors, symbol.market.timezone());
        Ok(bars)
    }

    /// 幂等：同一除权日重复写入只更新因子值
    pub fn upsert_adj_factors(
        &self,
        symbol: &Symbol,
        factors: &[AdjFactor],
    ) -> anyhow::Result<()> {
        let mut conn = self.conn.lock().expect("store 锁中毒");
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO adj_factors (symbol, effective_date, factor) VALUES (?1, ?2, ?3)
                 ON CONFLICT(symbol, effective_date) DO UPDATE SET factor = excluded.factor",
            )?;
            let sym = symbol.to_string();
            for a in factors {
                stmt.execute(rusqlite::params![
                    sym,
                    a.effective_date.to_string(),
                    a.factor
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 按除权日升序 —— `apply_factors` 要靠这个顺序做二分
    pub fn adj_factors(&self, symbol: &Symbol) -> anyhow::Result<Vec<AdjFactor>> {
        let conn = self.conn.lock().expect("store 锁中毒");
        let mut stmt = conn.prepare(
            "SELECT effective_date, factor FROM adj_factors WHERE symbol = ?1
             ORDER BY effective_date",
        )?;
        let rows = stmt.query_map([symbol.to_string()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (d, factor) = row?;
            match d.parse() {
                Ok(effective_date) => out.push(AdjFactor {
                    effective_date,
                    factor,
                }),
                // 坏掉的一条因子不该让整只标的画不出图
                Err(e) => eprintln!("[souba] 跳过无法解析的除权日 {d:?}：{e}"),
            }
        }
        Ok(out)
    }

    pub fn backfill_status(
        &self,
        symbol: &Symbol,
        tf: Timeframe,
    ) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().expect("store 锁中毒");
        let v = conn
            .query_row(
                "SELECT status FROM backfill_state WHERE symbol = ?1 AND timeframe = ?2",
                rusqlite::params![symbol.to_string(), tf.key()],
                |r| r.get::<_, String>(0),
            )
            .ok();
        Ok(v)
    }

    pub fn set_backfill_status(
        &self,
        symbol: &Symbol,
        tf: Timeframe,
        status: &str,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().expect("store 锁中毒");
        conn.execute(
            "INSERT INTO backfill_state (symbol, timeframe, status, updated_at)
             VALUES (?1, ?2, ?3, unixepoch())
             ON CONFLICT(symbol, timeframe) DO UPDATE SET
               status = excluded.status, updated_at = excluded.updated_at",
            rusqlite::params![symbol.to_string(), tf.key(), status],
        )?;
        Ok(())
    }

    /// AI 回答缓存。**只留在本地，不进 D1 同步** —— 丢了重问就是了，不值得占额度。
    pub fn ai_cached(&self, key: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().expect("store 锁中毒");
        Ok(conn
            .query_row("SELECT response FROM ai_cache WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn ai_cache_put(&self, key: &str, response: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().expect("store 锁中毒");
        conn.execute(
            "INSERT INTO ai_cache (key, response, created_at) VALUES (?1, ?2, unixepoch())
             ON CONFLICT(key) DO UPDATE SET response = excluded.response, created_at = excluded.created_at",
            rusqlite::params![key, response],
        )?;
        Ok(())
    }

    /// 阶段 2 的 d 键会调用它。现在自选股靠 seed 预置，还没有删除入口。
    #[allow(dead_code)]
    pub fn remove(&self, symbol: &Symbol) -> anyhow::Result<()> {
        let conn = self.conn.lock().expect("store 锁中毒");
        conn.execute(
            "DELETE FROM watchlist WHERE symbol = ?1",
            [symbol.to_string()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(s: &str) -> Symbol {
        Symbol::parse(s).unwrap()
    }

    #[test]
    fn 新库自选股为空() {
        let st = Store::open_in_memory().unwrap();
        assert!(st.watchlist().unwrap().is_empty());
    }

    #[test]
    fn 加入后能读回() {
        let st = Store::open_in_memory().unwrap();
        st.add(&sym("CN:600519"), "贵州茅台").unwrap();
        let list = st.watchlist().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].symbol.to_string(), "CN:600519");
        assert_eq!(list[0].name, "贵州茅台");
    }

    #[test]
    fn 保持加入顺序() {
        let st = Store::open_in_memory().unwrap();
        st.add(&sym("CN:600519"), "贵州茅台").unwrap();
        st.add(&sym("HK:00700"), "腾讯控股").unwrap();
        st.add(&sym("US:AAPL"), "苹果").unwrap();
        let codes: Vec<String> = st
            .watchlist()
            .unwrap()
            .iter()
            .map(|w| w.symbol.to_string())
            .collect();
        assert_eq!(codes, vec!["CN:600519", "HK:00700", "US:AAPL"]);
    }

    #[test]
    fn 重复加入不产生重复项且更新名称() {
        let st = Store::open_in_memory().unwrap();
        st.add(&sym("CN:600519"), "贵州茅台").unwrap();
        st.add(&sym("CN:600519"), "貴州茅台").unwrap();
        let list = st.watchlist().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "貴州茅台");
    }

    #[test]
    fn 设置写入后能读回并刷新_updated_at() {
        let st = Store::open_in_memory().unwrap();
        st.set_setting("vegas.fast", "144,169").unwrap();
        // 把时间戳抹成 0，再写一次，验证确实被刷新了
        st.conn
            .lock()
            .unwrap()
            .execute("UPDATE settings SET updated_at = 0", [])
            .unwrap();
        st.set_setting("vegas.fast", "89,144").unwrap();
        assert_eq!(
            st.all_settings().unwrap(),
            vec![("vegas.fast".to_string(), "89,144".to_string())]
        );
        let ts: i64 = st
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT updated_at FROM settings", [], |r| r.get(0))
            .unwrap();
        assert!(ts > 0, "updated_at 没被刷新");
    }

    #[test]
    fn 删除生效() {
        let st = Store::open_in_memory().unwrap();
        st.add(&sym("CN:600519"), "贵州茅台").unwrap();
        st.add(&sym("HK:00700"), "腾讯控股").unwrap();
        st.remove(&sym("CN:600519")).unwrap();
        let list = st.watchlist().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].symbol.to_string(), "HK:00700");
    }

    #[test]
    fn 删除不存在的不报错() {
        let st = Store::open_in_memory().unwrap();
        assert!(st.remove(&sym("CN:600519")).is_ok());
    }

    #[test]
    fn 库里存的是内部格式() {
        let st = Store::open_in_memory().unwrap();
        st.add(&sym("HK:700"), "腾讯控股").unwrap();
        // 港股补足五位后存储
        assert_eq!(st.watchlist().unwrap()[0].symbol.to_string(), "HK:00700");
    }


    fn sec(code: &str, name: &str, change_pct: f64, turnover: f64) -> Sector {
        Sector {
            code: code.into(),
            name: name.into(),
            kind: crate::core::sector::SectorKind::Industry,
            snapshot: Snapshot {
                change_pct,
                turnover,
            },
        }
    }

    fn count(st: &Store, table: &str) -> i64 {
        st.conn
            .lock()
            .unwrap()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn 板块与当日快照落库后可读回() {
        let st = Store::open_in_memory().unwrap();
        st.record_sectors(
            Market::Cn,
            "2026-09-18",
            &[sec("new_blhy", "玻璃行业", -0.96, 2.0e10)],
        )
        .unwrap();
        assert_eq!(count(&st, "sectors"), 1);
        let (name, kind): (String, String) = st
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT name, kind FROM sectors WHERE market = 'CN' AND code = 'new_blhy'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(name, "玻璃行业");
        assert_eq!(kind, "industry");
        // 当天自己不算历史，要用第二天的日期才读得到它
        let hist = st
            .sector_history(Market::Cn, "new_blhy", "2026-09-19", 5)
            .unwrap();
        assert_eq!(hist.len(), 1);
        assert!((hist[0].change_pct - -0.96).abs() < 1e-9);
        assert!((hist[0].turnover - 2.0e10).abs() < 1.0);
    }

    #[test]
    fn 同一天重跑不产生重复且覆盖数值() {
        let st = Store::open_in_memory().unwrap();
        let day = "2026-09-18";
        st.record_sectors(Market::Cn, day, &[sec("new_blhy", "玻璃行业", 1.0, 100.0)])
            .unwrap();
        st.record_sectors(Market::Cn, day, &[sec("new_blhy", "玻璃行业", 2.5, 300.0)])
            .unwrap();
        assert_eq!(count(&st, "sectors"), 1);
        assert_eq!(count(&st, "sector_daily"), 1);
        let hist = st
            .sector_history(Market::Cn, "new_blhy", "2026-09-19", 5)
            .unwrap();
        assert!((hist[0].change_pct - 2.5).abs() < 1e-9, "重跑应覆盖当天数值");
        assert!((hist[0].turnover - 300.0).abs() < 1e-9);
    }

    #[test]
    fn 板块改名只更新名称不动历史快照() {
        let st = Store::open_in_memory().unwrap();
        st.record_sectors(Market::Cn, "2026-09-17", &[sec("gn_x", "旧名", 1.0, 100.0)])
            .unwrap();
        st.record_sectors(Market::Cn, "2026-09-18", &[sec("gn_x", "新名", 2.0, 200.0)])
            .unwrap();
        assert_eq!(count(&st, "sectors"), 1);
        assert_eq!(count(&st, "sector_daily"), 2);
        let name: String = st
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT name FROM sectors WHERE code = 'gn_x'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "新名");
    }

    #[test]
    fn 历史按日期倒序且不含当天与未来() {
        let st = Store::open_in_memory().unwrap();
        for (d, c) in [
            ("2026-09-14", 1.0),
            ("2026-09-15", 2.0),
            ("2026-09-16", 3.0),
            ("2026-09-17", 4.0),
            ("2026-09-18", 5.0),
        ] {
            st.record_sectors(Market::Cn, d, &[sec("gn_x", "某概念", c, c * 100.0)])
                .unwrap();
        }
        let hist = st.sector_history(Market::Cn, "gn_x", "2026-09-17", 5).unwrap();
        let got: Vec<f64> = hist.iter().map(|s| s.change_pct).collect();
        assert_eq!(got, vec![3.0, 2.0, 1.0], "只要 09-17 之前的，且最近的在前");
    }

    #[test]
    fn 历史条数受限于请求天数() {
        let st = Store::open_in_memory().unwrap();
        for i in 1..=10 {
            st.record_sectors(
                Market::Cn,
                &format!("2026-09-{i:02}"),
                &[sec("gn_x", "某概念", i as f64, 100.0)],
            )
            .unwrap();
        }
        assert_eq!(
            st.sector_history(Market::Cn, "gn_x", "2026-09-20", 5).unwrap().len(),
            5
        );
        assert!(
            st.sector_history(Market::Cn, "gn_x", "2026-09-01", 5).unwrap().is_empty(),
            "没有更早的快照时应返回空而不是报错"
        );
    }

    #[test]
    fn 不同市场的同名板块互不干扰() {
        let st = Store::open_in_memory().unwrap();
        st.record_sectors(Market::Cn, "2026-09-17", &[sec("x", "A", 1.0, 100.0)])
            .unwrap();
        st.record_sectors(Market::Hk, "2026-09-17", &[sec("x", "B", 9.0, 900.0)])
            .unwrap();
        let hist = st.sector_history(Market::Cn, "x", "2026-09-18", 5).unwrap();
        assert_eq!(hist.len(), 1);
        assert!((hist[0].change_pct - 1.0).abs() < 1e-9);
    }

    // 真实响应：茅台全量日线的抽样（首尾各 20 根 + 几个对照日前后各 5 根）与全量 qfq 因子
    const CN_DAY: &[u8] = include_bytes!("../../tests/fixtures/sina_cn_kline_day_sh600519.json");
    const QFQ: &[u8] = include_bytes!("../../tests/fixtures/sina_cn_qfq_sh600519.js");

    fn maotai() -> Symbol {
        sym("CN:600519")
    }

    /// 把 fixture 里的原始日线与因子灌进一个空库
    fn seeded() -> Store {
        let st = Store::open_in_memory().unwrap();
        let bars =
            crate::source::history::parse_sina_cn(CN_DAY, chrono_tz::Asia::Shanghai).unwrap();
        st.upsert_bars(&maotai(), Timeframe::Day, &bars).unwrap();
        let factors = crate::source::history::parse_sina_qfq(QFQ).unwrap();
        st.upsert_adj_factors(&maotai(), &factors).unwrap();
        st
    }

    /// 取某个交易日那根 bar 的收盘价
    fn close_on(bars: &[Bar], day: &str) -> f64 {
        let d: chrono::NaiveDate = day.parse().unwrap();
        bars.iter()
            .find(|b| {
                b.ts.with_timezone(&chrono_tz::Asia::Shanghai).date_naive() == d
            })
            .unwrap_or_else(|| panic!("fixture 里没有 {day} 这一根"))
            .close
    }

    #[test]
    fn bars批量写入幂等() {
        let st = seeded();
        let n = st.raw_bars(&maotai(), Timeframe::Day).unwrap().len();
        assert!(n > 50, "fixture 应该有几十根，实际 {n}");
        let bars =
            crate::source::history::parse_sina_cn(CN_DAY, chrono_tz::Asia::Shanghai).unwrap();
        st.upsert_bars(&maotai(), Timeframe::Day, &bars).unwrap();
        assert_eq!(
            st.raw_bars(&maotai(), Timeframe::Day).unwrap().len(),
            n,
            "重复导入不该产生重复行"
        );
    }

    #[test]
    fn bars按周期分开存互不覆盖() {
        let st = seeded();
        let day_n = st.raw_bars(&maotai(), Timeframe::Day).unwrap().len();
        let one = st.raw_bars(&maotai(), Timeframe::Day).unwrap()[..1].to_vec();
        st.upsert_bars(&maotai(), Timeframe::Min60, &one).unwrap();
        assert_eq!(st.raw_bars(&maotai(), Timeframe::Day).unwrap().len(), day_n);
        assert_eq!(st.raw_bars(&maotai(), Timeframe::Min60).unwrap().len(), 1);
    }

    #[test]
    fn 读时复权对得上腾讯的前复权价() {
        // 新浪给原始价、腾讯给前复权价，两家算法不同（腾讯减分红、新浪除因子），
        // 所以只能要求接近。实测偏差见 core::adjust 的注释表。
        let st = seeded();
        let adj = st.adjusted_bars(&maotai(), Timeframe::Day).unwrap();
        for (day, tencent, tol) in [
            // 2024-06-14 那天离今天最远，两家算法的差也最大：实测 0.534%
            ("2024-06-14", 1420.661, 0.6),
            ("2024-06-21", 1367.537, 0.3),
            ("2025-06-20", 1349.079, 0.3),
            ("2025-06-27", 1351.109, 0.3),
        ] {
            let got = close_on(&adj, day);
            let dev = (got - tencent) / tencent * 100.0;
            assert!(
                dev.abs() < tol,
                "{day}：复权后 {got:.3}，腾讯 {tencent}，偏差 {dev:.3}% 超过 {tol}%"
            );
        }
    }

    #[test]
    fn 复权确实动了价格而不是原样返回() {
        let st = seeded();
        let raw = close_on(&st.raw_bars(&maotai(), Timeframe::Day).unwrap(), "2024-06-14");
        assert!((raw - 1555.0).abs() < 1e-6, "库里必须是原始价，实际 {raw}");
        let adj = close_on(&st.adjusted_bars(&maotai(), Timeframe::Day).unwrap(), "2024-06-14");
        assert!(adj < raw - 100.0, "复权后应明显低于原始价：{adj} vs {raw}");
    }

    #[test]
    fn 新增一条因子后重读结果变化而bars不变() {
        let st = seeded();
        let before_raw = st.raw_bars(&maotai(), Timeframe::Day).unwrap();
        let before = close_on(&st.adjusted_bars(&maotai(), Timeframe::Day).unwrap(), "2026-09-17");

        // 模拟又一次除权：2026-09-01 起累计因子变成 2
        st.upsert_adj_factors(
            &maotai(),
            &[crate::core::adjust::AdjFactor {
                effective_date: "2026-09-01".parse().unwrap(),
                factor: 2.0,
            }],
        )
        .unwrap();

        let after = close_on(&st.adjusted_bars(&maotai(), Timeframe::Day).unwrap(), "2026-09-17");
        assert!(
            (after - before / 2.0).abs() < 1e-9,
            "因子变了复权价就该跟着变：{before} → {after}"
        );
        assert!(
            before_raw == st.raw_bars(&maotai(), Timeframe::Day).unwrap(),
            "bars 行本身一个字节都不该动"
        );
        // 生效日之前的 bar 不受影响
        let old = close_on(&st.adjusted_bars(&maotai(), Timeframe::Day).unwrap(), "2024-06-14");
        assert!((old - 1413.081).abs() < 0.01, "早于生效日的 bar 不该受影响：{old}");
    }

    #[test]
    fn 因子upsert幂等且按日期升序读回() {
        let st = seeded();
        let n = st.adj_factors(&maotai()).unwrap().len();
        let again = crate::source::history::parse_sina_qfq(QFQ).unwrap();
        st.upsert_adj_factors(&maotai(), &again).unwrap();
        let got = st.adj_factors(&maotai()).unwrap();
        assert_eq!(got.len(), n, "同一除权日重复写入不该多出行");
        for w in got.windows(2) {
            assert!(w[0].effective_date < w[1].effective_date);
        }
    }

    #[test]
    fn 没有因子时复权价等于原始价() {
        let st = Store::open_in_memory().unwrap();
        let bars =
            crate::source::history::parse_sina_cn(CN_DAY, chrono_tz::Asia::Shanghai).unwrap();
        st.upsert_bars(&maotai(), Timeframe::Day, &bars).unwrap();
        assert_eq!(
            st.raw_bars(&maotai(), Timeframe::Day).unwrap(),
            st.adjusted_bars(&maotai(), Timeframe::Day).unwrap()
        );
    }

    #[test]
    fn 空库读出空序列而不是报错() {
        let st = Store::open_in_memory().unwrap();
        assert!(st.raw_bars(&maotai(), Timeframe::Day).unwrap().is_empty());
        assert!(st.adjusted_bars(&maotai(), Timeframe::Day).unwrap().is_empty());
        assert!(st.backfill_status(&maotai(), Timeframe::Day).unwrap().is_none());
    }

    #[test]
    fn 回补状态可写可读可覆盖() {
        let st = Store::open_in_memory().unwrap();
        st.set_backfill_status(&maotai(), Timeframe::Day, "running").unwrap();
        assert_eq!(
            st.backfill_status(&maotai(), Timeframe::Day).unwrap().as_deref(),
            Some("running")
        );
        st.set_backfill_status(&maotai(), Timeframe::Day, "done").unwrap();
        assert_eq!(
            st.backfill_status(&maotai(), Timeframe::Day).unwrap().as_deref(),
            Some("done")
        );
        // 周期是主键的一部分，别互相踩
        assert!(st.backfill_status(&maotai(), Timeframe::Min60).unwrap().is_none());
    }

    /// 阶段 1 的 schema，冻结在测试里当 fixture —— 它描述的是「线上旧库长什么样」，
    /// 不该跟着 MIGRATIONS 一起变，否则升级测试就自证其说了。
    const PHASE1_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS watchlist (
  symbol     TEXT PRIMARY KEY,
  name       TEXT NOT NULL,
  sort_order INTEGER NOT NULL,
  added_at   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS settings (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS bars (
  symbol    TEXT NOT NULL,
  timeframe TEXT NOT NULL,
  ts        INTEGER NOT NULL,
  open REAL NOT NULL, high REAL NOT NULL, low REAL NOT NULL,
  close REAL NOT NULL, volume REAL NOT NULL,
  adjusted  INTEGER NOT NULL,
  PRIMARY KEY (symbol, timeframe, ts)
) WITHOUT ROWID;
"#;

    /// 造一个阶段 1 的旧库（user_version 仍为 0）并塞 3 条自选股，再交给 Store 打开
    fn open_phase1_db() -> Store {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(PHASE1_SCHEMA).unwrap();
        conn.execute_batch(
            "INSERT INTO watchlist (symbol, name, sort_order, added_at) VALUES
               ('CN:600519', '贵州茅台', 0, 1000),
               ('HK:00700',  '腾讯控股', 1, 1001),
               ('US:AAPL',   '苹果',     2, 1002);
             INSERT INTO settings (key, value) VALUES ('vegas.fast', '144,169');",
        )
        .unwrap();
        Store::from_conn(conn).unwrap()
    }

    fn user_version(st: &Store) -> i64 {
        st.conn
            .lock()
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    /// sqlite_master 全量快照，用来比对两条路径建出来的 schema
    fn schema_dump(st: &Store) -> Vec<(String, String, String)> {
        let conn = st.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT type, name, COALESCE(sql, '') FROM sqlite_master
                 WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    const NEW_TABLES: &[&str] = &[
        "adj_factors",
        "sectors",
        "sector_daily",
        "sector_members",
        "scan_results",
        "ai_cache",
        "backfill_state",
        "sync_state",
    ];

    #[test]
    fn 旧库升级后自选股一条不丢() {
        let st = open_phase1_db();
        let codes: Vec<String> = st
            .watchlist()
            .unwrap()
            .iter()
            .map(|w| w.symbol.to_string())
            .collect();
        assert_eq!(codes, vec!["CN:600519", "HK:00700", "US:AAPL"]);
        assert_eq!(st.watchlist().unwrap()[0].name, "贵州茅台");
        // settings 也不该被迁移清掉
        let v: String = st
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT value FROM settings WHERE key = 'vegas.fast'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(v, "144,169");
    }

    #[test]
    fn 旧库升级后版本为最新() {
        assert_eq!(user_version(&open_phase1_db()), MIGRATIONS.len() as i64);
    }

    #[test]
    fn 升级后所有新表可查询() {
        let st = open_phase1_db();
        let conn = st.conn.lock().unwrap();
        for t in NEW_TABLES {
            let n: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))
                .unwrap_or_else(|e| panic!("新表 {t} 查不了：{e}"));
            assert_eq!(n, 0);
        }
    }

    #[test]
    fn bars_不再有_adjusted_列() {
        let st = open_phase1_db();
        let conn = st.conn.lock().unwrap();
        let cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('bars')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(!cols.contains(&"adjusted".to_string()), "实际列：{cols:?}");
        assert!(cols.contains(&"close".to_string()));
    }

    #[test]
    fn watchlist_与_settings_有_updated_at_且既有行已填时间() {
        let st = open_phase1_db();
        let conn = st.conn.lock().unwrap();
        for t in ["watchlist", "settings"] {
            let n: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {t} WHERE updated_at > 0"),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(n > 0, "{t} 的既有行 updated_at 没填");
        }
    }

    #[test]
    fn 新库与升级后的库_schema_一致() {
        let fresh = Store::open_in_memory().unwrap();
        let upgraded = open_phase1_db();
        assert_eq!(user_version(&fresh), user_version(&upgraded));
        assert_eq!(schema_dump(&fresh), schema_dump(&upgraded));
    }

    #[test]
    fn 重复迁移不动已有数据() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO bars (symbol, timeframe, ts, open, high, low, close, volume)
             VALUES ('CN:600519', 'day', 1, 1.0, 2.0, 0.5, 1.5, 100.0);",
        )
        .unwrap();
        migrate(&conn).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM bars", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "重跑迁移把 bars 清空了");
    }

    #[test]
    fn ai缓存可写可读未命中返回none() {
        let st = Store::open_in_memory().unwrap();
        assert_eq!(st.ai_cached("没写过").unwrap(), None);
        st.ai_cache_put("k1", "第一次的回答").unwrap();
        assert_eq!(st.ai_cached("k1").unwrap().as_deref(), Some("第一次的回答"));
        // 同键重写覆盖而不是报唯一约束
        st.ai_cache_put("k1", "重问的回答").unwrap();
        assert_eq!(st.ai_cached("k1").unwrap().as_deref(), Some("重问的回答"));
    }
}
