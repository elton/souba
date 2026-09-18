use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::Connection;

use crate::core::symbol::Symbol;

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
}
