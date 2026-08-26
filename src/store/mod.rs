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

/// 与 Cloudflare D1 共用的 schema。bars 表阶段 1 还没用到，
/// 但先建好 —— 阶段 4 要往里灌历史，届时不必迁移。
const SCHEMA: &str = r#"
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
        conn.execute_batch(SCHEMA)?;
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
}
