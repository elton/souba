-- 阶段 1 的基线 schema。本地旧库已经是这个样子（user_version=0），
-- D1 是空的，所以这里从零建一遍，再由 0002 升到最新。
-- 本地 SQLite 与 D1 共用这些文件，Rust 侧 include_str! 引用，避免两边漂移。

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
