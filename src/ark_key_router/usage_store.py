from __future__ import annotations

import os
import sqlite3
import threading
import time
from datetime import date, datetime, timedelta
from pathlib import Path


EMPTY_BUCKET = {
    "requests": 0,
    "errors": 0,
    "prompt_tokens": 0,
    "cached_tokens": 0,
    "completion_tokens": 0,
    "total_tokens": 0,
    "cache_hit_rate": 0.0,
}


class UsageStore:
    def __init__(self, db_path: str):
        self.db_path = os.path.expanduser(db_path)
        if self.db_path != ":memory:":
            Path(self.db_path).parent.mkdir(parents=True, exist_ok=True)
        self._lock = threading.Lock()
        self._conn = sqlite3.connect(self.db_path, check_same_thread=False)
        self._conn.row_factory = sqlite3.Row
        self._init_db()

    def _init_db(self) -> None:
        with self._lock, self._conn:
            self._conn.execute(
                """
                CREATE TABLE IF NOT EXISTS usage_meta (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                )
                """
            )
            self._conn.execute(
                """
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
                )
                """
            )
            self._ensure_column("usage_events", "cached_tokens", "INTEGER NOT NULL DEFAULT 0")
            self._conn.execute("CREATE INDEX IF NOT EXISTS idx_usage_model ON usage_events(model)")
            self._conn.execute("CREATE INDEX IF NOT EXISTS idx_usage_key ON usage_events(key_name)")
            self._conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_usage_status ON usage_events(status_code)"
            )
            self._conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_usage_created_at ON usage_events(created_at)"
            )
            self._conn.execute(
                "INSERT OR IGNORE INTO usage_meta(key, value) VALUES ('started_at', ?)",
                (str(time.time()),),
            )

    def record(
        self,
        *,
        model: str,
        key_name: str,
        status_code: int,
        usage: dict | None,
    ) -> None:
        usage = usage or {}
        with self._lock, self._conn:
            self._conn.execute(
                """
                INSERT INTO usage_events(
                    created_at,
                    model,
                    key_name,
                    status_code,
                    prompt_tokens,
                    cached_tokens,
                    completion_tokens,
                    total_tokens
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    time.time(),
                    model,
                    key_name,
                    status_code,
                    int(usage.get("prompt_tokens") or 0),
                    extract_cached_tokens(usage),
                    int(usage.get("completion_tokens") or 0),
                    int(usage.get("total_tokens") or 0),
                ),
            )

    def reset(self) -> None:
        with self._lock, self._conn:
            self._conn.execute("DELETE FROM usage_events")
            self._conn.execute(
                "UPDATE usage_meta SET value = ? WHERE key = 'started_at'",
                (str(time.time()),),
            )

    def snapshot(
        self,
        *,
        period: str = "all",
        start: str | None = None,
        end: str | None = None,
    ) -> dict:
        with self._lock:
            started_at = self._started_at()
            range_start, range_end = resolve_time_range(period, start, end)
            where_sql, args = time_filter_sql(range_start, range_end)
            return {
                "started_at": int(started_at),
                "uptime_seconds": int(time.time() - started_at),
                "range": {
                    "period": period,
                    "start": int(range_start) if range_start is not None else None,
                    "end": int(range_end) if range_end is not None else None,
                },
                "total": self._bucket(
                    f"""
                    SELECT
                        COUNT(*) AS requests,
                        COALESCE(SUM(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END), 0) AS errors,
                        COALESCE(SUM(prompt_tokens), 0) AS prompt_tokens,
                        COALESCE(SUM(cached_tokens), 0) AS cached_tokens,
                        COALESCE(SUM(completion_tokens), 0) AS completion_tokens,
                        COALESCE(SUM(total_tokens), 0) AS total_tokens
                    FROM usage_events
                    {where_sql}
                    """,
                    args,
                ),
                "by_model": self._grouped("model", where_sql, args),
                "by_key": self._grouped("key_name", where_sql, args),
                "by_status": self._grouped("status_code", where_sql, args),
                "by_day": self._timeseries("day", range_start, range_end),
                "by_month": self._timeseries("month", range_start, range_end),
                "db_path": self.db_path,
            }

    def _ensure_column(self, table: str, column: str, definition: str) -> None:
        rows = self._conn.execute(f"PRAGMA table_info({table})").fetchall()
        if any(row["name"] == column for row in rows):
            return
        self._conn.execute(f"ALTER TABLE {table} ADD COLUMN {column} {definition}")

    def _started_at(self) -> float:
        row = self._conn.execute("SELECT value FROM usage_meta WHERE key = 'started_at'").fetchone()
        if row is None:
            return time.time()
        return float(row["value"])

    def _bucket(self, query: str, args: tuple = ()) -> dict:
        row = self._conn.execute(query, args).fetchone()
        if row is None:
            return dict(EMPTY_BUCKET)
        bucket = {
            "requests": int(row["requests"] or 0),
            "errors": int(row["errors"] or 0),
            "prompt_tokens": int(row["prompt_tokens"] or 0),
            "cached_tokens": int(row["cached_tokens"] or 0),
            "completion_tokens": int(row["completion_tokens"] or 0),
            "total_tokens": int(row["total_tokens"] or 0),
        }
        bucket["cache_hit_rate"] = cache_hit_rate(bucket)
        return bucket

    def _grouped(self, column: str, where_sql: str, args: tuple) -> dict[str, dict]:
        rows = self._conn.execute(
            f"""
            SELECT
                {column} AS name,
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
            """,
            args,
        ).fetchall()
        return {str(row["name"]): self._bucket_from_row(row) for row in rows}

    def _timeseries(self, bucket: str, start: float | None, end: float | None) -> dict[str, dict]:
        where_sql, args = time_filter_sql(start, end)
        format_expr = "%Y-%m-%d" if bucket == "day" else "%Y-%m"
        rows = self._conn.execute(
            f"""
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
            """,
            args,
        ).fetchall()
        return {str(row["name"]): self._bucket_from_row(row) for row in rows}

    def _bucket_from_row(self, row: sqlite3.Row) -> dict:
        bucket = {
            "requests": int(row["requests"] or 0),
            "errors": int(row["errors"] or 0),
            "prompt_tokens": int(row["prompt_tokens"] or 0),
            "cached_tokens": int(row["cached_tokens"] or 0),
            "completion_tokens": int(row["completion_tokens"] or 0),
            "total_tokens": int(row["total_tokens"] or 0),
        }
        bucket["cache_hit_rate"] = cache_hit_rate(bucket)
        return bucket


def extract_cached_tokens(usage: dict) -> int:
    details = usage.get("prompt_tokens_details")
    if isinstance(details, dict):
        return int(details.get("cached_tokens") or 0)
    return int(usage.get("cached_tokens") or 0)


def cache_hit_rate(bucket: dict) -> float:
    prompt_tokens = int(bucket.get("prompt_tokens") or 0)
    if prompt_tokens <= 0:
        return 0.0
    return round(int(bucket.get("cached_tokens") or 0) / prompt_tokens, 4)


def resolve_time_range(
    period: str,
    start: str | None,
    end: str | None,
) -> tuple[float | None, float | None]:
    now = datetime.now().astimezone()
    if start or end:
        return parse_time_value(start), parse_time_value(end, end_of_day=True)
    if period == "today":
        day_start = datetime.combine(now.date(), datetime.min.time(), tzinfo=now.tzinfo)
        return day_start.timestamp(), None
    if period == "day":
        return (now - timedelta(days=1)).timestamp(), None
    if period == "month":
        month_start = datetime(now.year, now.month, 1, tzinfo=now.tzinfo)
        return month_start.timestamp(), None
    return None, None


def parse_time_value(value: str | None, *, end_of_day: bool = False) -> float | None:
    if not value:
        return None
    if value.isdigit():
        return float(value)
    if len(value) == 10:
        parsed_date = date.fromisoformat(value)
        parsed_time = datetime.max.time() if end_of_day else datetime.min.time()
        return datetime.combine(parsed_date, parsed_time).astimezone().timestamp()
    return datetime.fromisoformat(value).astimezone().timestamp()


def time_filter_sql(start: float | None, end: float | None) -> tuple[str, tuple]:
    clauses: list[str] = []
    args: list[float] = []
    if start is not None:
        clauses.append("created_at >= ?")
        args.append(start)
    if end is not None:
        clauses.append("created_at <= ?")
        args.append(end)
    if not clauses:
        return "", ()
    return "WHERE " + " AND ".join(clauses), tuple(args)
