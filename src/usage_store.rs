use crate::state_store::now_seconds;
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveDateTime, TimeZone};
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Bucket {
    pub requests: i64,
    pub errors: i64,
    pub prompt_tokens: i64,
    pub cached_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub cache_hit_rate: f64,
}

pub struct UsageStore {
    conn: Connection,
    pub db_path: String,
}

impl UsageStore {
    pub fn new(path: &str) -> anyhow::Result<Self> {
        let (conn, db_path) = if path == ":memory:" {
            (Connection::open_in_memory()?, ":memory:".to_string())
        } else {
            let path_buf = crate::config::expand_path(path);
            if let Some(parent) = path_buf.parent() {
                fs::create_dir_all(parent)?;
            }
            (
                Connection::open(&path_buf)?,
                path_buf.to_string_lossy().to_string(),
            )
        };
        let store = Self { conn, db_path };
        store.init_db()?;
        Ok(store)
    }

    fn init_db(&self) -> anyhow::Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS usage_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS usage_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at REAL NOT NULL,
                model TEXT NOT NULL,
                key_name TEXT NOT NULL,
                status_code INTEGER NOT NULL,
                prompt_tokens INTEGER NOT NULL DEFAULT 0,
                cached_tokens INTEGER NOT NULL DEFAULT 0,
                completion_tokens INTEGER NOT NULL DEFAULT 0,
                total_tokens INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_usage_model ON usage_events(model);
            CREATE INDEX IF NOT EXISTS idx_usage_key ON usage_events(key_name);
            CREATE INDEX IF NOT EXISTS idx_usage_status ON usage_events(status_code);
            CREATE INDEX IF NOT EXISTS idx_usage_created_at ON usage_events(created_at);
            "#,
        )?;
        ensure_column(
            &self.conn,
            "usage_events",
            "cached_tokens",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        self.conn.execute(
            "INSERT OR IGNORE INTO usage_meta(key, value) VALUES ('started_at', ?)",
            params![now_seconds().to_string()],
        )?;
        Ok(())
    }

    pub fn record(
        &self,
        model: &str,
        key_name: &str,
        status_code: u16,
        usage: Option<&Value>,
    ) -> anyhow::Result<()> {
        let prompt_tokens = usage
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let completion_tokens = usage
            .and_then(|u| u.get("completion_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let total_tokens = usage
            .and_then(|u| u.get("total_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let cached_tokens = extract_cached_tokens(usage);
        self.conn.execute(
            r#"
            INSERT INTO usage_events(
                created_at, model, key_name, status_code,
                prompt_tokens, cached_tokens, completion_tokens, total_tokens
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            "#,
            params![
                now_seconds(),
                model,
                key_name,
                i64::from(status_code),
                prompt_tokens,
                cached_tokens,
                completion_tokens,
                total_tokens
            ],
        )?;
        Ok(())
    }

    pub fn reset(&self) -> anyhow::Result<()> {
        self.conn.execute("DELETE FROM usage_events", [])?;
        self.conn.execute(
            "UPDATE usage_meta SET value = ? WHERE key = 'started_at'",
            params![now_seconds().to_string()],
        )?;
        Ok(())
    }

    pub fn snapshot(
        &self,
        period: &str,
        start: Option<&str>,
        end: Option<&str>,
    ) -> anyhow::Result<Value> {
        let started_at = self.started_at()?;
        let (range_start, range_end) = resolve_time_range(period, start, end);
        let (where_sql, args) = time_filter_sql(range_start, range_end);
        Ok(json!({
            "started_at": started_at as i64,
            "uptime_seconds": (now_seconds() - started_at).max(0.0) as i64,
            "range": {
                "period": period,
                "start": range_start.map(|value| value as i64),
                "end": range_end.map(|value| value as i64),
            },
            "total": self.bucket(&format!(r#"
                SELECT
                    COUNT(*) AS requests,
                    COALESCE(SUM(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END), 0) AS errors,
                    COALESCE(SUM(prompt_tokens), 0) AS prompt_tokens,
                    COALESCE(SUM(cached_tokens), 0) AS cached_tokens,
                    COALESCE(SUM(completion_tokens), 0) AS completion_tokens,
                    COALESCE(SUM(total_tokens), 0) AS total_tokens
                FROM usage_events
                {where_sql}
            "#), &args)?,
            "by_model": self.grouped("model", &where_sql, &args)?,
            "by_key": self.grouped("key_name", &where_sql, &args)?,
            "by_status": self.grouped("status_code", &where_sql, &args)?,
            "by_day": self.timeseries("day", range_start, range_end)?,
            "by_month": self.timeseries("month", range_start, range_end)?,
            "db_path": self.db_path,
        }))
    }

    pub fn key_token_totals_for_model(
        &self,
        model: &str,
        key_names: &[String],
    ) -> anyhow::Result<HashMap<String, i64>> {
        let mut totals = key_names
            .iter()
            .map(|name| (name.clone(), 0))
            .collect::<HashMap<_, _>>();
        if key_names.is_empty() {
            return Ok(totals);
        }
        let (range_start, range_end) = resolve_time_range("today", None, None);
        let (where_sql, mut args) = time_filter_sql(range_start, range_end);
        let placeholders = key_names.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let conjunction = if where_sql.is_empty() { "WHERE" } else { "AND" };
        let query = format!(
            r#"
            SELECT key_name, COALESCE(SUM(total_tokens), 0) AS total_tokens
            FROM usage_events
            {where_sql}
            {conjunction} model = ? AND key_name IN ({placeholders})
            GROUP BY key_name
            "#,
        );
        args.push(SqlValue::Text(model.to_string()));
        args.extend(key_names.iter().cloned().map(SqlValue::Text));
        let mut stmt = self.conn.prepare(&query)?;
        let rows = stmt.query_map(params_from_iter(args.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (name, total) = row?;
            totals.insert(name, total);
        }
        Ok(totals)
    }

    fn started_at(&self) -> anyhow::Result<f64> {
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM usage_meta WHERE key = 'started_at'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or_else(now_seconds))
    }

    fn bucket(&self, query: &str, args: &[SqlValue]) -> anyhow::Result<Bucket> {
        let mut stmt = self.conn.prepare(query)?;
        let bucket = stmt.query_row(params_from_iter(args.iter()), |row| {
            let mut bucket = Bucket {
                requests: row.get::<_, i64>("requests")?,
                errors: row.get::<_, i64>("errors")?,
                prompt_tokens: row.get::<_, i64>("prompt_tokens")?,
                cached_tokens: row.get::<_, i64>("cached_tokens")?,
                completion_tokens: row.get::<_, i64>("completion_tokens")?,
                total_tokens: row.get::<_, i64>("total_tokens")?,
                cache_hit_rate: 0.0,
            };
            bucket.cache_hit_rate = cache_hit_rate(&bucket);
            Ok(bucket)
        })?;
        Ok(bucket)
    }

    fn grouped(
        &self,
        column: &str,
        where_sql: &str,
        args: &[SqlValue],
    ) -> anyhow::Result<HashMap<String, Bucket>> {
        let query = format!(
            r#"
            SELECT
                CAST({column} AS TEXT) AS name,
                COUNT(*) AS requests,
                COALESCE(SUM(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END), 0) AS errors,
                COALESCE(SUM(prompt_tokens), 0) AS prompt_tokens,
                COALESCE(SUM(cached_tokens), 0) AS cached_tokens,
                COALESCE(SUM(completion_tokens), 0) AS completion_tokens,
                COALESCE(SUM(total_tokens), 0) AS total_tokens
            FROM usage_events
            {where_sql}
            GROUP BY {column}
            ORDER BY {column}
            "#,
        );
        let mut stmt = self.conn.prepare(&query)?;
        let rows = stmt.query_map(params_from_iter(args.iter()), |row| {
            let mut bucket = Bucket {
                requests: row.get::<_, i64>("requests")?,
                errors: row.get::<_, i64>("errors")?,
                prompt_tokens: row.get::<_, i64>("prompt_tokens")?,
                cached_tokens: row.get::<_, i64>("cached_tokens")?,
                completion_tokens: row.get::<_, i64>("completion_tokens")?,
                total_tokens: row.get::<_, i64>("total_tokens")?,
                cache_hit_rate: 0.0,
            };
            bucket.cache_hit_rate = cache_hit_rate(&bucket);
            Ok((row.get::<_, String>("name")?, bucket))
        })?;
        let mut result = HashMap::new();
        for row in rows {
            let (name, bucket) = row?;
            result.insert(name, bucket);
        }
        Ok(result)
    }

    fn timeseries(
        &self,
        bucket: &str,
        start: Option<f64>,
        end: Option<f64>,
    ) -> anyhow::Result<HashMap<String, Bucket>> {
        let (where_sql, args) = time_filter_sql(start, end);
        let format_expr = if bucket == "day" { "%Y-%m-%d" } else { "%Y-%m" };
        let query = format!(
            r#"
            SELECT
                strftime('{format_expr}', created_at, 'unixepoch', 'localtime') AS name,
                COUNT(*) AS requests,
                COALESCE(SUM(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END), 0) AS errors,
                COALESCE(SUM(prompt_tokens), 0) AS prompt_tokens,
                COALESCE(SUM(cached_tokens), 0) AS cached_tokens,
                COALESCE(SUM(completion_tokens), 0) AS completion_tokens,
                COALESCE(SUM(total_tokens), 0) AS total_tokens
            FROM usage_events
            {where_sql}
            GROUP BY name
            ORDER BY name
            "#,
        );
        self.grouped_timeseries(&query, &args)
    }

    fn grouped_timeseries(
        &self,
        query: &str,
        args: &[SqlValue],
    ) -> anyhow::Result<HashMap<String, Bucket>> {
        let mut stmt = self.conn.prepare(query)?;
        let rows = stmt.query_map(params_from_iter(args.iter()), |row| {
            let mut bucket = Bucket {
                requests: row.get::<_, i64>("requests")?,
                errors: row.get::<_, i64>("errors")?,
                prompt_tokens: row.get::<_, i64>("prompt_tokens")?,
                cached_tokens: row.get::<_, i64>("cached_tokens")?,
                completion_tokens: row.get::<_, i64>("completion_tokens")?,
                total_tokens: row.get::<_, i64>("total_tokens")?,
                cache_hit_rate: 0.0,
            };
            bucket.cache_hit_rate = cache_hit_rate(&bucket);
            Ok((row.get::<_, String>("name")?, bucket))
        })?;
        let mut result = HashMap::new();
        for row in rows {
            let (name, bucket) = row?;
            result.insert(name, bucket);
        }
        Ok(result)
    }
}

fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> anyhow::Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(());
        }
    }
    conn.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
        [],
    )?;
    Ok(())
}

fn extract_cached_tokens(usage: Option<&Value>) -> i64 {
    usage
        .and_then(|u| u.get("prompt_tokens_details"))
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_i64)
        .or_else(|| {
            usage
                .and_then(|u| u.get("cached_tokens"))
                .and_then(Value::as_i64)
        })
        .unwrap_or(0)
}

fn cache_hit_rate(bucket: &Bucket) -> f64 {
    if bucket.prompt_tokens <= 0 {
        0.0
    } else {
        ((bucket.cached_tokens as f64 / bucket.prompt_tokens as f64) * 10_000.0).round() / 10_000.0
    }
}

fn resolve_time_range(
    period: &str,
    start: Option<&str>,
    end: Option<&str>,
) -> (Option<f64>, Option<f64>) {
    if start.is_some() || end.is_some() {
        return (parse_time_value(start, false), parse_time_value(end, true));
    }
    let now = Local::now();
    match period {
        "today" => {
            let start = Local
                .with_ymd_and_hms(now.year(), now.month(), now.day(), 0, 0, 0)
                .single();
            (start.map(|dt| dt.timestamp() as f64), None)
        }
        "day" => (Some((now - Duration::days(1)).timestamp() as f64), None),
        "month" => {
            let start = Local
                .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
                .single();
            (start.map(|dt| dt.timestamp() as f64), None)
        }
        _ => (None, None),
    }
}

fn parse_time_value(value: Option<&str>, end_of_day: bool) -> Option<f64> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    if value.chars().all(|ch| ch.is_ascii_digit()) {
        return value.parse::<f64>().ok();
    }
    if value.len() == 10 {
        let date = NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()?;
        let (h, m, s) = if end_of_day { (23, 59, 59) } else { (0, 0, 0) };
        return Local
            .from_local_datetime(&date.and_hms_opt(h, m, s)?)
            .single()
            .map(|dt| dt.timestamp() as f64);
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Some(dt.timestamp() as f64);
    }
    if let Ok(naive) = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S") {
        return Local
            .from_local_datetime(&naive)
            .single()
            .map(|dt| dt.timestamp() as f64);
    }
    None
}

fn time_filter_sql(start: Option<f64>, end: Option<f64>) -> (String, Vec<SqlValue>) {
    let mut clauses = Vec::new();
    let mut args = Vec::new();
    if let Some(start) = start {
        clauses.push("created_at >= ?");
        args.push(SqlValue::Real(start));
    }
    if let Some(end) = end {
        clauses.push("created_at <= ?");
        args.push(SqlValue::Real(end));
    }
    if clauses.is_empty() {
        (String::new(), args)
    } else {
        (format!("WHERE {}", clauses.join(" AND ")), args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_snapshot_casts_status_codes_to_string_keys() {
        let store = UsageStore::new(":memory:").unwrap();
        store.record("glm-latest-auto", "hevin", 200, None).unwrap();
        store.record("glm-latest-auto", "hevin", 599, None).unwrap();

        let snapshot = store.snapshot("all", None, None).unwrap();

        assert!(snapshot["by_status"].get("200").is_some());
        assert!(snapshot["by_status"].get("599").is_some());
    }
}
