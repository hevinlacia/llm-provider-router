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
    pub prompt_uncached_tokens: i64,
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
        ensure_column(&self.conn, "usage_events", "session_id", "TEXT")?;
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

    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &self,
        model: &str,
        key_name: &str,
        status_code: u16,
        usage: Option<&Value>,
        session_id: Option<&str>,
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
        let session_id = session_id.filter(|value| !value.is_empty());
        self.conn.execute(
            r#"
            INSERT INTO usage_events(
                created_at, model, key_name, status_code, session_id,
                prompt_tokens, cached_tokens, completion_tokens, total_tokens
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
            params![
                now_seconds(),
                model,
                key_name,
                i64::from(status_code),
                session_id,
                prompt_tokens,
                cached_tokens,
                completion_tokens,
                total_tokens,
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

    /// 活跃 session 聚合：近 window_secs 内有交互的 session_id 分组，
    /// 统计 60s / 5min / 全窗三个时间窗的输出 token 与请求错误数。
    /// 无 session_id 的事件合并进 `unidentified` 一行，便于前端展示识别覆盖率。
    pub fn active_sessions(&self, window_secs: i64, now: f64) -> anyhow::Result<Value> {
        let t60 = now - 60.0;
        let t5m = now - 300.0;
        let t1h = now - window_secs as f64;

        // SQLite 裸列 + MAX() 语义：非聚合列取自 MAX(created_at) 所在行（即该 session 最近一次请求的 key）。
        let mut stmt = self.conn.prepare(
            r#"
            SELECT
                COALESCE(session_id, ''),
                MAX(created_at),
                COUNT(*),
                COALESCE(SUM(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN created_at > ?1 THEN completion_tokens ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN created_at > ?2 THEN completion_tokens ELSE 0 END), 0),
                COALESCE(SUM(completion_tokens), 0),
                key_name
            FROM usage_events
            WHERE created_at > ?3
            GROUP BY (session_id IS NULL), session_id
            ORDER BY MAX(created_at) DESC
            "#,
        )?;
        let rows = stmt.query_map(params![t60, t5m, t1h], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        })?;
        let mut sessions: Vec<Value> = Vec::new();
        let mut unidentified: Option<Value> = None;
        for row in rows {
            let (session_id, last_at, requests, errors, out60, out5m, out1h, key_name) = row?;
            let mut item = json!({
                "session_id": session_id,
                "last_activity": last_at,
                "requests": requests,
                "errors": errors,
                "output_tokens": { "60s": out60, "5m": out5m, "1h": out1h },
                "key_name": key_name,
            });
            if session_id.is_empty() {
                if let Some(obj) = item.as_object_mut() {
                    obj.remove("session_id");
                }
                unidentified = Some(item);
            } else {
                sessions.push(item);
            }
        }

        // 每个 session 用到的 model（按 token 降序，最多 3 个）
        let mut models_stmt = self.conn.prepare(
            r#"
            SELECT session_id, model, SUM(total_tokens) AS tokens
            FROM usage_events
            WHERE created_at > ?1 AND session_id IS NOT NULL
            GROUP BY session_id, model
            ORDER BY tokens DESC
            "#,
        )?;
        let mut models_by_session: HashMap<String, Vec<String>> = HashMap::new();
        let model_rows = models_stmt.query_map(params![t1h], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in model_rows {
            let (session_id, model) = row?;
            let list = models_by_session.entry(session_id).or_default();
            if list.len() < 3 {
                list.push(model);
            }
        }
        for session in &mut sessions {
            if let Some(id) = session.get("session_id").and_then(Value::as_str) {
                if let Some(models) = models_by_session.get(id) {
                    session["models"] = json!(models);
                }
            }
        }

        Ok(json!({
            "window_seconds": window_secs,
            "active_threshold_seconds": 300,
            "sessions": sessions,
            "unidentified": unidentified,
        }))
    }

    pub fn snapshot(
        &self,
        period: &str,
        start: Option<&str>,
        end: Option<&str>,
        key_names: Option<&[String]>,
    ) -> anyhow::Result<Value> {
        let started_at = self.started_at()?;
        let (range_start, range_end) = resolve_time_range(period, start, end);
        let (where_sql, args) =
            apply_key_filter(time_filter_sql(range_start, range_end), key_names);
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
            "by_day": self.timeseries("day", &where_sql, &args)?,
            "by_month": self.timeseries("month", &where_sql, &args)?,
            "db_path": self.db_path,
        }))
    }

    /// Series: 小时/天维度的可视趋势；支持按 model/key_name 分组，按维度 topN 兜底聚合为"other"。
    ///
    /// 用途：新增用量分析页的“供应商/模型 token 曲线”主数据源；不破坏 snapshot 契约。
    #[allow(clippy::too_many_arguments)]
    pub fn series(
        &self,
        period: &str,
        start: Option<&str>,
        end: Option<&str>,
        bucket: &str,
        group_by: &str,
        top: Option<usize>,
        key_names: Option<&[String]>,
    ) -> anyhow::Result<Value> {
        let (range_start, range_end) = resolve_time_range(period, start, end);
        let bucket_expr = match bucket {
            "hour" => "%Y-%m-%dT%H:00:00",
            "month" => "%Y-%m",
            _ => "%Y-%m-%d",
        };
        let group_col = match group_by {
            "model" => "model",
            "key" | "key_name" => "key_name",
            "provider" => "key_name",
            _ => "model",
        };
        // 预计算真实 range 起止的 unix 秒（考虑 period 隐含的相对 range）
        let (where_sql, where_args) =
            apply_key_filter(time_filter_sql(range_start, range_end), key_names);
        // 拉分维时序
        let series = self.grouped_series(&bucket_expr, group_col, &where_sql, &where_args)?;
        // 维度合并：topN=0 表示全保留；否则取 total_tokens 最高的 N 个，其余归 other
        let top_n = top.unwrap_or(0);
        let merged = if top_n > 0 {
            merge_top_groups(series, top_n)
        } else {
            series
        };
        // 补 0：为所有 bucket 统一做“该桶为 0 也保留”的对齐（利于前端做连续曲线）
        let buckets: Vec<String> = {
            let mut b = merged
                .values()
                .flat_map(|m| m.keys().cloned())
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            b.sort();
            b
        };
        let filled = fill_zero_buckets(merged, &buckets);
        Ok(json!({
            "bucket": bucket,
            "group_by": group_by,
            "buckets": buckets,
            "series": filled,
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
                prompt_uncached_tokens: 0,
                completion_tokens: row.get::<_, i64>("completion_tokens")?,
                total_tokens: row.get::<_, i64>("total_tokens")?,
                cache_hit_rate: 0.0,
            };
            bucket.prompt_uncached_tokens = prompt_uncached_tokens(&bucket);
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
                prompt_uncached_tokens: 0,
                completion_tokens: row.get::<_, i64>("completion_tokens")?,
                total_tokens: row.get::<_, i64>("total_tokens")?,
                cache_hit_rate: 0.0,
            };
            bucket.prompt_uncached_tokens = prompt_uncached_tokens(&bucket);
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
        where_sql: &str,
        args: &[SqlValue],
    ) -> anyhow::Result<HashMap<String, Bucket>> {
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
        self.grouped_timeseries(&query, args)
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
                prompt_uncached_tokens: 0,
                completion_tokens: row.get::<_, i64>("completion_tokens")?,
                total_tokens: row.get::<_, i64>("total_tokens")?,
                cache_hit_rate: 0.0,
            };
            bucket.prompt_uncached_tokens = prompt_uncached_tokens(&bucket);
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

    fn grouped_series(
        &self,
        bucket_expr: &str,
        group_col: &str,
        where_sql: &str,
        args: &[SqlValue],
    ) -> anyhow::Result<HashMap<String, HashMap<String, Bucket>>> {
        let query = format!(
            r#"
            SELECT
                strftime('{bucket_expr}', created_at, 'unixepoch', 'localtime') AS bucket,
                CAST({group_col} AS TEXT) AS name,
                COUNT(*) AS requests,
                COALESCE(SUM(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END), 0) AS errors,
                COALESCE(SUM(prompt_tokens), 0) AS prompt_tokens,
                COALESCE(SUM(cached_tokens), 0) AS cached_tokens,
                COALESCE(SUM(completion_tokens), 0) AS completion_tokens,
                COALESCE(SUM(total_tokens), 0) AS total_tokens
            FROM usage_events
            {where_sql}
            GROUP BY bucket, name
            ORDER BY bucket, name
            "#,
        );
        let mut stmt = self.conn.prepare(&query)?;
        let rows = stmt.query_map(params_from_iter(args.iter()), |row| {
            let mut bucket = Bucket {
                requests: row.get::<_, i64>("requests")?,
                errors: row.get::<_, i64>("errors")?,
                prompt_tokens: row.get::<_, i64>("prompt_tokens")?,
                cached_tokens: row.get::<_, i64>("cached_tokens")?,
                prompt_uncached_tokens: 0,
                completion_tokens: row.get::<_, i64>("completion_tokens")?,
                total_tokens: row.get::<_, i64>("total_tokens")?,
                cache_hit_rate: 0.0,
            };
            bucket.prompt_uncached_tokens = prompt_uncached_tokens(&bucket);
            bucket.cache_hit_rate = cache_hit_rate(&bucket);
            let b: String = row.get("bucket")?;
            let n: String = row.get("name")?;
            Ok((n, b, bucket))
        })?;
        let mut result: HashMap<String, HashMap<String, Bucket>> = HashMap::new();
        for row in rows {
            let (name, bucket, data) = row?;
            result.entry(name).or_default().insert(bucket, data);
        }
        Ok(result)
    }
}

fn merge_top_groups(
    mut series: HashMap<String, HashMap<String, Bucket>>,
    top: usize,
) -> HashMap<String, HashMap<String, Bucket>> {
    let order: Vec<(String, i64)> = series
        .iter()
        .map(|(k, m)| (k.clone(), m.values().map(|b| b.total_tokens).sum::<i64>()))
        .collect();
    let mut ranked = order;
    ranked.sort_by(|a, b| b.1.cmp(&a.1));
    if ranked.len() <= top {
        return series;
    }
    let keep: std::collections::HashSet<String> =
        ranked.into_iter().take(top).map(|(k, _)| k).collect();
    let other_keys: Vec<String> = series
        .keys()
        .filter(|k| !keep.contains(*k))
        .cloned()
        .collect();
    let mut other: HashMap<String, Bucket> = HashMap::new();
    for k in &other_keys {
        if let Some(m) = series.remove(k) {
            for (bucket, b) in m {
                let cur = other.entry(bucket).or_insert_with(|| Bucket {
                    requests: 0,
                    errors: 0,
                    prompt_tokens: 0,
                    cached_tokens: 0,
                    prompt_uncached_tokens: 0,
                    completion_tokens: 0,
                    total_tokens: 0,
                    cache_hit_rate: 0.0,
                });
                cur.requests += b.requests;
                cur.errors += b.errors;
                cur.prompt_tokens += b.prompt_tokens;
                cur.cached_tokens += b.cached_tokens;
                cur.prompt_uncached_tokens += b.prompt_uncached_tokens;
                cur.completion_tokens += b.completion_tokens;
                cur.total_tokens += b.total_tokens;
            }
        }
    }
    for bucket in other.values_mut() {
        bucket.cache_hit_rate = cache_hit_rate(bucket);
    }
    if !other.is_empty() {
        series.insert("other".to_string(), other);
    }
    series
}

fn fill_zero_buckets(
    mut series: HashMap<String, HashMap<String, Bucket>>,
    buckets: &[String],
) -> HashMap<String, HashMap<String, Bucket>> {
    for m in series.values_mut() {
        for b in buckets {
            m.entry(b.clone()).or_insert_with(|| Bucket {
                requests: 0,
                errors: 0,
                prompt_tokens: 0,
                cached_tokens: 0,
                prompt_uncached_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                cache_hit_rate: 0.0,
            });
        }
    }
    series
}

/// 在现有时间过滤 SQL 上追加 `key_name IN (...)` 过滤（供应商维度明细下钻用）。
/// `None` 表示不过滤；`Some(空)` 表示指定了供应商但无匹配 key，应过滤为空集。
fn apply_key_filter(
    (where_sql, args): (String, Vec<SqlValue>),
    key_names: Option<&[String]>,
) -> (String, Vec<SqlValue>) {
    let Some(key_names) = key_names else {
        return (where_sql, args);
    };
    let mut where_sql = where_sql;
    let mut args = args;
    if key_names.is_empty() {
        where_sql = if where_sql.is_empty() {
            "WHERE 1 = 0".to_string()
        } else {
            format!("{} AND 1 = 0", where_sql.trim())
        };
        return (where_sql, args);
    }
    let placeholders = key_names.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let conjunction = if where_sql.is_empty() { "WHERE" } else { "AND" };
    where_sql = format!(
        "{} {conjunction} key_name IN ({placeholders})",
        where_sql.trim()
    );
    args.extend(key_names.iter().cloned().map(SqlValue::Text));
    (where_sql.trim().to_string(), args)
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

fn prompt_uncached_tokens(bucket: &Bucket) -> i64 {
    (bucket.prompt_tokens - bucket.cached_tokens).max(0)
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
        store
            .record("glm-latest-auto", "hevin", 200, None, None)
            .unwrap();
        store
            .record("glm-latest-auto", "hevin", 599, None, None)
            .unwrap();

        let snapshot = store.snapshot("all", None, None, None).unwrap();

        assert!(snapshot["by_status"].get("200").is_some());
        assert!(snapshot["by_status"].get("599").is_some());
    }

    #[test]
    fn usage_snapshot_splits_cached_and_uncached_prompt_tokens() {
        let store = UsageStore::new(":memory:").unwrap();
        let usage = json!({
            "prompt_tokens": 100,
            "prompt_tokens_details": { "cached_tokens": 40 },
            "completion_tokens": 25,
            "total_tokens": 125
        });
        store
            .record("glm-latest-auto", "hevin", 200, Some(&usage), None)
            .unwrap();

        let snapshot = store.snapshot("all", None, None, None).unwrap();

        assert_eq!(snapshot["total"]["cached_tokens"], 40);
        assert_eq!(snapshot["total"]["prompt_uncached_tokens"], 60);
        assert_eq!(snapshot["total"]["completion_tokens"], 25);
    }

    #[test]
    fn series_filters_by_provider_key_names() {
        let store = UsageStore::new(":memory:").unwrap();
        store
            .record("glm-latest-auto", "providerA/key1", 200, None, None)
            .unwrap();
        store
            .record("glm-latest-auto", "providerA/key2", 200, None, None)
            .unwrap();
        store
            .record("deepseek-chat", "providerB/key3", 200, None, None)
            .unwrap();

        let keys = vec!["providerA/key1".to_string(), "providerA/key2".to_string()];
        let series = store
            .series("all", None, None, "day", "key", Some(0), Some(&keys))
            .unwrap();
        let obj = series["series"].as_object().unwrap();

        assert!(obj.contains_key("providerA/key1"));
        assert!(obj.contains_key("providerA/key2"));
        assert!(!obj.contains_key("providerB/key3"));
    }

    #[test]
    fn series_empty_key_filter_returns_empty() {
        let store = UsageStore::new(":memory:").unwrap();
        store
            .record("glm-latest-auto", "providerA/key1", 200, None, None)
            .unwrap();

        let series = store
            .series("all", None, None, "day", "key", Some(0), Some(&[]))
            .unwrap();
        let obj = series["series"].as_object().unwrap();

        assert!(obj.is_empty());
    }

    #[test]
    fn active_sessions_groups_sessions_and_unidentified() {
        let store = UsageStore::new(":memory:").unwrap();
        let usage_a = json!({ "prompt_tokens": 10, "completion_tokens": 100, "total_tokens": 110 });
        let usage_b = json!({ "prompt_tokens": 10, "completion_tokens": 30, "total_tokens": 40 });
        store
            .record("model-a", "ark/k1", 200, Some(&usage_a), Some("sess-aaa"))
            .unwrap();
        store
            .record("model-a", "ark/k1", 200, Some(&usage_a), Some("sess-aaa"))
            .unwrap();
        store
            .record(
                "model-b",
                "opencode/k2",
                200,
                Some(&usage_b),
                Some("sess-bbb"),
            )
            .unwrap();
        // 无 session 标识的错误请求 → unidentified 聚合行
        store.record("model-c", "ark/k1", 500, None, None).unwrap();

        let result = store.active_sessions(3600, now_seconds()).unwrap();

        let sessions = result["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 2);
        let aaa = sessions
            .iter()
            .find(|s| s["session_id"] == "sess-aaa")
            .unwrap();
        assert_eq!(aaa["requests"], 2);
        assert_eq!(aaa["output_tokens"]["60s"], 200);
        assert_eq!(aaa["output_tokens"]["1h"], 200);
        assert_eq!(aaa["models"][0], "model-a");
        assert_eq!(aaa["key_name"], "ark/k1");
        let bbb = sessions
            .iter()
            .find(|s| s["session_id"] == "sess-bbb")
            .unwrap();
        assert_eq!(bbb["output_tokens"]["5m"], 30);

        let unidentified = result["unidentified"].as_object().unwrap();
        assert_eq!(unidentified["requests"], 1);
        assert_eq!(unidentified["errors"], 1);
    }
}
