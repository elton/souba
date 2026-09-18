-- A 股闭环：bars 去复权列、板块与扫描结果、同步游标。
-- 对应 docs/specs/2026-09-18-A股闭环.md 的 §Schema。

-- bars 存原始价 + adj_factors 读时复权，adjusted 列那条路不走了。
-- 旧库这张表是空的，直接重建。
DROP TABLE IF EXISTS bars;
CREATE TABLE bars (
  symbol    TEXT NOT NULL,
  timeframe TEXT NOT NULL,
  ts        INTEGER NOT NULL,
  open REAL NOT NULL, high REAL NOT NULL, low REAL NOT NULL,
  close REAL NOT NULL, volume REAL NOT NULL,
  PRIMARY KEY (symbol, timeframe, ts)
) WITHOUT ROWID;

-- watchlist / settings 加 updated_at，D1 同步按它「最后写入者胜」。
-- ALTER TABLE ADD COLUMN 的默认值必须是常量，取不到 unixepoch()，所以整表重建；
-- 既有行的 updated_at 填当前时间。
CREATE TABLE watchlist_new (
  symbol     TEXT PRIMARY KEY,
  name       TEXT NOT NULL,
  sort_order INTEGER NOT NULL,
  added_at   INTEGER NOT NULL,
  updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);
INSERT INTO watchlist_new (symbol, name, sort_order, added_at, updated_at)
  SELECT symbol, name, sort_order, added_at, unixepoch() FROM watchlist;
DROP TABLE watchlist;
ALTER TABLE watchlist_new RENAME TO watchlist;

CREATE TABLE settings_new (
  key        TEXT PRIMARY KEY,
  value      TEXT NOT NULL,
  updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);
INSERT INTO settings_new (key, value, updated_at)
  SELECT key, value, unixepoch() FROM settings;
DROP TABLE settings;
ALTER TABLE settings_new RENAME TO settings;

-- 新浪 qfq.js 的累计前复权因子：effective_date 之前的 bar 除以该因子
CREATE TABLE adj_factors (
  symbol         TEXT NOT NULL,
  effective_date TEXT NOT NULL,
  factor         REAL NOT NULL,
  PRIMARY KEY (symbol, effective_date)
) WITHOUT ROWID;

-- kind ∈ 行业 / 概念；后续市场自行扩展取值，不改 schema
CREATE TABLE sectors (
  market     TEXT NOT NULL,
  code       TEXT NOT NULL,
  name       TEXT NOT NULL,
  kind       TEXT NOT NULL,
  updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
  PRIMARY KEY (market, code)
) WITHOUT ROWID;

-- 板块每日快照，热度公式的输入
CREATE TABLE sector_daily (
  market      TEXT NOT NULL,
  sector_code TEXT NOT NULL,
  date        TEXT NOT NULL,
  change_pct  REAL NOT NULL,
  turnover    REAL NOT NULL,
  PRIMARY KEY (market, sector_code, date)
) WITHOUT ROWID;

-- 当日抓到的板块内涨幅前 K 只
CREATE TABLE sector_members (
  market      TEXT NOT NULL,
  sector_code TEXT NOT NULL,
  symbol      TEXT NOT NULL,
  name        TEXT NOT NULL,
  as_of       TEXT NOT NULL,
  rank        INTEGER NOT NULL,
  PRIMARY KEY (market, sector_code, as_of, symbol)
) WITHOUT ROWID;

-- 机会面板直接读这张表；freshness 仅 Long 有意义，其余为 NULL
CREATE TABLE scan_results (
  date        TEXT NOT NULL,
  market      TEXT NOT NULL,
  sector_code TEXT NOT NULL,
  symbol      TEXT NOT NULL,
  rank        INTEGER NOT NULL,
  stance      TEXT NOT NULL,
  freshness   INTEGER,
  facets_json TEXT NOT NULL,
  PRIMARY KEY (date, market, sector_code, symbol)
) WITHOUT ROWID;

-- 只在本地用，不进同步（D1 上建出来是共用迁移的副作用，同步路由不碰它）。
-- key = 输入 JSON 的 SHA-256 + 日期
CREATE TABLE ai_cache (
  key        TEXT PRIMARY KEY,
  response   TEXT NOT NULL,
  created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

-- 回补进度，支持断点续补
CREATE TABLE backfill_state (
  symbol     TEXT NOT NULL,
  timeframe  TEXT NOT NULL,
  status     TEXT NOT NULL,
  updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
  PRIMARY KEY (symbol, timeframe)
) WITHOUT ROWID;

-- 每张表的最后同步时刻
CREATE TABLE sync_state (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
