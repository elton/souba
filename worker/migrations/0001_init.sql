-- souba schema —— 本地 SQLite 与 D1 共用同一份 DDL。
-- 列名与主键改动必须两边同时改，否则同步会静默错位。

CREATE TABLE IF NOT EXISTS watchlist (
  symbol     TEXT PRIMARY KEY,
  name       TEXT NOT NULL,
  sort_order INTEGER NOT NULL,
  added_at   INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS settings (
  key        TEXT PRIMARY KEY,
  value      TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);

-- 永远存原始价，只增不改 —— 所以同步就是双向 upsert，不存在冲突。
-- 复权在读取时用 adj_factors 算，不落库。
CREATE TABLE IF NOT EXISTS bars (
  symbol    TEXT NOT NULL,
  timeframe TEXT NOT NULL,
  ts        INTEGER NOT NULL,
  open   REAL NOT NULL,
  high   REAL NOT NULL,
  low    REAL NOT NULL,
  close  REAL NOT NULL,
  volume REAL NOT NULL,
  PRIMARY KEY (symbol, timeframe, ts)
);
CREATE INDEX IF NOT EXISTS idx_bars_ts ON bars (ts);

CREATE TABLE IF NOT EXISTS adj_factors (
  symbol         TEXT NOT NULL,
  effective_date TEXT NOT NULL,
  factor         REAL NOT NULL,
  PRIMARY KEY (symbol, effective_date)
);

CREATE TABLE IF NOT EXISTS sectors (
  market     TEXT NOT NULL,
  code       TEXT NOT NULL,
  name       TEXT NOT NULL,
  kind       TEXT NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (market, code)
);

CREATE TABLE IF NOT EXISTS sector_daily (
  market      TEXT NOT NULL,
  sector_code TEXT NOT NULL,
  date        TEXT NOT NULL,
  change_pct  REAL NOT NULL,
  turnover    REAL NOT NULL,
  PRIMARY KEY (market, sector_code, date)
);

CREATE TABLE IF NOT EXISTS sector_members (
  market      TEXT NOT NULL,
  sector_code TEXT NOT NULL,
  symbol      TEXT NOT NULL,
  name        TEXT NOT NULL,
  as_of       TEXT NOT NULL,
  rank        INTEGER NOT NULL,
  PRIMARY KEY (market, sector_code, as_of, symbol)
);

CREATE TABLE IF NOT EXISTS scan_results (
  date        TEXT NOT NULL,
  market      TEXT NOT NULL,
  sector_code TEXT NOT NULL,
  symbol      TEXT NOT NULL,
  rank        INTEGER NOT NULL,
  stance      TEXT NOT NULL,
  freshness   INTEGER,
  facets_json TEXT NOT NULL,
  PRIMARY KEY (date, market, sector_code, symbol)
);

-- 本地缓存，不参与同步；放在同一份迁移里只是为了两边 schema 完全一致。
CREATE TABLE IF NOT EXISTS ai_cache (
  key        TEXT PRIMARY KEY,
  response   TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS backfill_state (
  symbol     TEXT NOT NULL,
  timeframe  TEXT NOT NULL,
  status     TEXT NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (symbol, timeframe)
);

-- 同步游标，以及 Cron 的「某市场今天已追加到哪一天」。
CREATE TABLE IF NOT EXISTS sync_state (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
