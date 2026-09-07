-- KunxOS 云服务 D1 数据库 schema
-- 用法: wrangler d1 execute kunxos-cloud --remote --file=schema.sql

-- 设备表
CREATE TABLE IF NOT EXISTS devices (
  device_id TEXT PRIMARY KEY,
  account TEXT,
  name TEXT,
  token_hash TEXT,
  ip TEXT,
  last_seen INTEGER,
  created INTEGER
);

-- 备份表（每个设备整快照替换：先 DELETE 后 INSERT）
CREATE TABLE IF NOT EXISTS backups (
  device_id TEXT,
  path TEXT,
  data TEXT,
  updated INTEGER,
  PRIMARY KEY (device_id, path)
);

-- 命令表
CREATE TABLE IF NOT EXISTS commands (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  device_id TEXT,
  type TEXT,
  payload TEXT,
  status TEXT DEFAULT 'pending',
  result TEXT DEFAULT '',
  created INTEGER,
  acked INTEGER
);
