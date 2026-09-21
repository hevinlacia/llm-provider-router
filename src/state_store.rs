use crate::config::expand_path;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct StateStore {
    conn: Connection,
}

impl StateStore {
    pub fn new(path: &str) -> anyhow::Result<Self> {
        if path != ":memory:" {
            let path = expand_path(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let conn = Connection::open(path)?;
            let store = Self { conn };
            store.init_db()?;
            return Ok(store);
        }
        let store = Self {
            conn: Connection::open_in_memory()?,
        };
        store.init_db()?;
        Ok(store)
    }

    fn init_db(&self) -> anyhow::Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS frozen_keys (
                key_name TEXT PRIMARY KEY,
                until REAL NOT NULL,
                reason TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS session_bindings (
                alias TEXT NOT NULL,
                session_id TEXT NOT NULL,
                key_name TEXT NOT NULL,
                expires_at REAL NOT NULL,
                PRIMARY KEY (alias, session_id)
            );
            CREATE INDEX IF NOT EXISTS idx_bindings_expires ON session_bindings(expires_at);
            CREATE TABLE IF NOT EXISTS key_model_unsupported (
                provider TEXT NOT NULL,
                key_name TEXT NOT NULL,
                model TEXT NOT NULL,
                last_error TEXT NOT NULL,
                attempt INTEGER NOT NULL,
                last_error_at REAL NOT NULL,
                retry_at REAL NOT NULL,
                permanent INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (provider, key_name, model)
            );
            "#,
        )?;
        Ok(())
    }

    pub fn load_frozen(&self) -> anyhow::Result<HashMap<String, (f64, String)>> {
        let now = now_seconds();
        let mut stmt = self
            .conn
            .prepare("SELECT key_name, until, reason FROM frozen_keys WHERE until > ?")?;
        let rows = stmt.query_map(params![now], |row| {
            Ok((
                row.get::<_, String>(0)?,
                (row.get::<_, f64>(1)?, row.get::<_, String>(2)?),
            ))
        })?;
        let mut result = HashMap::new();
        for row in rows {
            let (name, item) = row?;
            result.insert(name, item);
        }
        Ok(result)
    }

    pub fn upsert_frozen(&self, key_name: &str, until: f64, reason: &str) -> anyhow::Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO frozen_keys(key_name, until, reason)
            VALUES (?, ?, ?)
            ON CONFLICT(key_name) DO UPDATE
              SET until = excluded.until, reason = excluded.reason
              WHERE excluded.until > frozen_keys.until
            "#,
            params![key_name, until, reason],
        )?;
        Ok(())
    }

    pub fn delete_frozen(&self, key_names: &[String]) -> anyhow::Result<()> {
        for key_name in key_names {
            self.conn.execute(
                "DELETE FROM frozen_keys WHERE key_name = ?",
                params![key_name],
            )?;
        }
        Ok(())
    }

    pub fn clear_frozen(&self) -> anyhow::Result<()> {
        self.conn.execute("DELETE FROM frozen_keys", [])?;
        Ok(())
    }

    pub fn load_unsupported(
        &self,
    ) -> anyhow::Result<HashMap<(String, String, String), crate::features::router::UnsupportedEntry>>
    {
        let mut stmt = self.conn.prepare(
            "SELECT provider, key_name, model, last_error, attempt, last_error_at, retry_at, permanent
             FROM key_model_unsupported",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                (
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ),
                crate::features::router::UnsupportedEntry {
                    last_error: row.get(3)?,
                    attempt: row.get(4)?,
                    last_error_at: row.get(5)?,
                    retry_at: row.get(6)?,
                    permanent: row.get::<_, i64>(7)? != 0,
                },
            ))
        })?;
        let mut result = HashMap::new();
        for row in rows {
            let (key, item) = row?;
            result.insert(key, item);
        }
        Ok(result)
    }

    pub fn upsert_unsupported(
        &self,
        provider: &str,
        key_name: &str,
        model: &str,
        entry: &crate::features::router::UnsupportedEntry,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO key_model_unsupported(provider, key_name, model, last_error, attempt, last_error_at, retry_at, permanent)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(provider, key_name, model) DO UPDATE
              SET last_error = excluded.last_error,
                  attempt = excluded.attempt,
                  last_error_at = excluded.last_error_at,
                  retry_at = excluded.retry_at,
                  permanent = excluded.permanent
            "#,
            params![
                provider,
                key_name,
                model,
                entry.last_error,
                entry.attempt,
                entry.last_error_at,
                entry.retry_at,
                entry.permanent as i64,
            ],
        )?;
        Ok(())
    }

    /// 按 (provider, key, model) 前缀过滤删除；参数为 None 表示该维度不限制。返回删除行数。
    pub fn delete_unsupported(
        &self,
        provider: Option<&str>,
        key_name: Option<&str>,
        model: Option<&str>,
    ) -> anyhow::Result<usize> {
        let affected = match (provider, key_name, model) {
            (None, None, None) => self.conn.execute("DELETE FROM key_model_unsupported", [])?,
            (Some(p), None, None) => self
                .conn
                .execute("DELETE FROM key_model_unsupported WHERE provider = ?", params![p])?,
            (Some(p), Some(k), None) => self.conn.execute(
                "DELETE FROM key_model_unsupported WHERE provider = ? AND key_name = ?",
                params![p, k],
            )?,
            (Some(p), Some(k), Some(m)) => self.conn.execute(
                "DELETE FROM key_model_unsupported WHERE provider = ? AND key_name = ? AND model = ?",
                params![p, k, m],
            )?,
            (None, Some(k), None) => self
                .conn
                .execute("DELETE FROM key_model_unsupported WHERE key_name = ?", params![k])?,
            (None, None, Some(m)) => self
                .conn
                .execute("DELETE FROM key_model_unsupported WHERE model = ?", params![m])?,
            (None, Some(k), Some(m)) => self.conn.execute(
                "DELETE FROM key_model_unsupported WHERE key_name = ? AND model = ?",
                params![k, m],
            )?,
            (Some(p), None, Some(m)) => self.conn.execute(
                "DELETE FROM key_model_unsupported WHERE provider = ? AND model = ?",
                params![p, m],
            )?,
        };
        Ok(affected)
    }

    pub fn load_bindings(&self) -> anyhow::Result<HashMap<(String, String), (String, f64)>> {
        let now = now_seconds();
        let mut stmt = self.conn.prepare(
            "SELECT alias, session_id, key_name, expires_at FROM session_bindings WHERE expires_at > ?",
        )?;
        let rows = stmt.query_map(params![now], |row| {
            Ok((
                (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                (row.get::<_, String>(2)?, row.get::<_, f64>(3)?),
            ))
        })?;
        let mut result = HashMap::new();
        for row in rows {
            let (key, value) = row?;
            result.insert(key, value);
        }
        Ok(result)
    }

    pub fn upsert_binding(
        &self,
        alias: &str,
        session_id: &str,
        key_name: &str,
        expires_at: f64,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO session_bindings(alias, session_id, key_name, expires_at)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(alias, session_id) DO UPDATE
              SET key_name = excluded.key_name, expires_at = excluded.expires_at
            "#,
            params![alias, session_id, key_name, expires_at],
        )?;
        Ok(())
    }

    pub fn delete_bindings(&self, keys: &[(String, String)]) -> anyhow::Result<()> {
        for (alias, session_id) in keys {
            self.conn.execute(
                "DELETE FROM session_bindings WHERE alias = ? AND session_id = ?",
                params![alias, session_id],
            )?;
        }
        Ok(())
    }
}

pub fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
