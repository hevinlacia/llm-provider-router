//! 客户端流量统计（纯内存，秒桶滑动窗口）：session 级 + 全局的上/下行字节。
//!
//! 目标（2026-10-27）：dashboard 可看每个 session 的瞬时与 5s/10s/60s 平均上下行
//! 流量，以及 router 全局累计与速率——**热路径只做计数，不做持久化**：
//! - 上行（client→router）：/v1 handler 在拿到请求体时记一次（Content-Length，
//!   delta 请求即真实 wire 字节）；
//! - 下行（router→client）：/v1 响应体中间件逐 chunk 记（压缩后 wire 字节）；
//! - 聚合：每秒一个桶的环形缓冲（64 槽覆盖 60s 窗口），读时求和，写 O(1)；
//! - 内存：session 上限 [`MAX_SESSIONS`]（超出按 last_activity 淘汰），全局常驻
//!   两个结构，总量 <1MB；
//! - 并发：单把 Mutex 保护（session 数与流并发都很小，锁持有 ~百 ns，无影响）。
//!
//! 边界：/api/* 管理面流量不计（非 LLM 流量）；/v1/search 等无 session 端点只进
//! 全局；session 归属失败（无标识且指纹缺失）只进全局。

use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;

/// 秒桶槽数：覆盖 60s 窗口（取模复用，靠桶内时间戳判新旧）。
const SLOTS: usize = 64;
/// session 上限：超过按最近活跃淘汰（正常个位数 session，防御异常客户端刷爆内存）。
const MAX_SESSIONS: usize = 512;

/// 纯秒桶滑动窗口：用户要求的三档口径 1s（瞬时）/ 5s / 1min（60s）。

/// 单组秒桶（全局与每个 session 各一份）。
#[derive(Clone, Copy)]
struct Buckets {
    /// 桶对应的 unix 秒；`sec[i] != 当前桶秒` 时视为空桶。
    sec: [u64; SLOTS],
    up: [u64; SLOTS],
    down: [u64; SLOTS],
}

impl Buckets {
    fn new() -> Self {
        Self {
            sec: [0; SLOTS],
            up: [0; SLOTS],
            down: [0; SLOTS],
        }
    }

    /// 记一笔到 `now` 秒的桶。桶被复用（64s 前的旧桶）时先清零。
    fn add(&mut self, now: u64, up: u64, down: u64) {
        let i = (now as usize) % SLOTS;
        if self.sec[i] != now {
            self.sec[i] = now;
            self.up[i] = 0;
            self.down[i] = 0;
        }
        self.up[i] = self.up[i].saturating_add(up);
        self.down[i] = self.down[i].saturating_add(down);
    }

    /// `secs` 窗口内字节总和（含 now 秒的当前桶，不含已滚出窗口的桶）。
    fn window(&self, now: u64, secs: u64) -> (u64, u64) {
        let mut up = 0u64;
        let mut down = 0u64;
        for i in 0..SLOTS {
            let s = self.sec[i];
            if s != 0 && s + secs > now && s <= now {
                up = up.saturating_add(self.up[i]);
                down = down.saturating_add(self.down[i]);
            }
        }
        (up, down)
    }
}

struct SessionEntry {
    buckets: Buckets,
    total_up: u64,
    total_down: u64,
    last_activity: u64,
}

struct Inner {
    sessions: HashMap<String, SessionEntry>,
    global_buckets: Buckets,
    global_total_up: u64,
    global_total_down: u64,
}

/// 全局流量注册表。AppState 持 `Arc<Traffic>`。
pub(crate) struct Traffic {
    inner: Mutex<Inner>,
}

impl Traffic {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                sessions: HashMap::new(),
                global_buckets: Buckets::new(),
                global_total_up: 0,
                global_total_down: 0,
            }),
        }
    }

    /// 记一次请求的上行字节（请求时调用一次；down 走 [`Self::record_down_chunk`]）。
    /// `session` 为 None 时只进全局。
    pub(crate) fn record_up(&self, session: Option<&str>, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let now = now_sec();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.global_total_up = inner.global_total_up.saturating_add(bytes);
        inner.global_buckets.add(now, bytes, 0);
        if let Some(sid) = session.filter(|s| !s.is_empty()) {
            let entry = touch_session(&mut inner.sessions, sid, now);
            entry.total_up = entry.total_up.saturating_add(bytes);
            entry.buckets.add(now, bytes, 0);
        }
    }

    /// 记一个下行 chunk（响应体流式回调；粒度为 axum body frame）。
    pub(crate) fn record_down_chunk(&self, session: Option<&str>, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let now = now_sec();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.global_total_down = inner.global_total_down.saturating_add(bytes);
        inner.global_buckets.add(now, 0, bytes);
        if let Some(sid) = session.filter(|s| !s.is_empty()) {
            let entry = touch_session(&mut inner.sessions, sid, now);
            entry.total_down = entry.total_down.saturating_add(bytes);
            entry.buckets.add(now, 0, bytes);
        }
    }

    /// API 快照：全局累计/速率 + 各 session 明细。
    pub(crate) fn snapshot(&self) -> Value {
        let now = now_sec();
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        let mut sessions = serde_json::Map::new();
        for (sid, entry) in &inner.sessions {
            sessions.insert(sid.clone(), entry_view(&entry.buckets, entry.total_up, entry.total_down, now));
        }

        serde_json::json!({
            "window_seconds": 60,
            "global": entry_view(&inner.global_buckets, inner.global_total_up, inner.global_total_down, now),
            "sessions": Value::Object(sessions),
        })
    }
}

fn entry_view(buckets: &Buckets, total_up: u64, total_down: u64, now: u64) -> Value {
    let (up1, down1) = buckets.window(now, 1);
    let (up5, down5) = buckets.window(now, 5);
    let (up60, down60) = buckets.window(now, 60);
    let rate = |bytes: u64, secs: u64| bytes as f64 / secs as f64;
    serde_json::json!({
        "total_up": total_up,
        "total_down": total_down,
        "rates": {
            "up": { "1s": rate(up1, 1), "5s": rate(up5, 5), "60s": rate(up60, 60) },
            "down": { "1s": rate(down1, 1), "5s": rate(down5, 5), "60s": rate(down60, 60) },
        },
    })
}

/// 取/建 session 条目，超限时淘汰最久不活跃的（近似 LRU；MAX_SESSIONS 量级全扫可忽略）。
fn touch_session<'a>(
    sessions: &'a mut HashMap<String, SessionEntry>,
    sid: &str,
    now: u64,
) -> &'a mut SessionEntry {
    if !sessions.contains_key(sid) && sessions.len() >= MAX_SESSIONS {
        if let Some(oldest) = sessions
            .iter()
            .min_by_key(|(_, e)| e.last_activity)
            .map(|(k, _)| k.clone())
        {
            sessions.remove(&oldest);
        }
    }
    sessions.entry(sid.to_string()).or_insert_with(|| SessionEntry {
        buckets: Buckets::new(),
        total_up: 0,
        total_down: 0,
        last_activity: now,
    })
}

fn now_sec() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手工推时序的桶求和：窗口边界与旧桶过期都要对。
    #[test]
    fn buckets_window_math() {
        let mut b = Buckets::new();
        b.add(1000, 100, 0);
        b.add(1001, 200, 0);
        b.add(1005, 400, 50);
        // 5s 窗口 (1000..=1005]？定义：s + secs > now && s <= now → s in (now-secs, now]
        let (up, down) = b.window(1005, 5); // 1001..1005 → 200+400
        assert_eq!((up, down), (600, 50));
        let (up, _) = b.window(1005, 10); // 全部 3 桶
        assert_eq!(up, 700);
        // 60s 后旧桶自然过期
        let (up, _) = b.window(1065, 60); // >1005 的桶才计入 → 0
        assert_eq!(up, 0);
    }

    /// 桶槽复用：64s 后同一槽位被覆盖，旧数据不得泄漏进窗口。
    #[test]
    fn buckets_slot_reuse_no_leak() {
        let mut b = Buckets::new();
        b.add(1000, 999, 0);
        let i = 1000 % SLOTS;
        assert_eq!(b.up[i], 999);
        b.add(1000 + SLOTS as u64, 7, 0); // 同槽新秒
        assert_eq!(b.up[i], 7, "复用槽应清零重写");
        let (up, _) = b.window(1000 + SLOTS as u64, 60);
        assert_eq!(up, 7, "旧秒数据不应计入");
    }

    #[test]
    fn traffic_up_down_totals_and_rates() {
        let t = Traffic::new();
        t.record_up(Some("s1"), 1000);
        t.record_up(Some("s1"), 500);
        t.record_up(None, 10); // 无归属 → 只进全局
        t.record_down_chunk(Some("s1"), 300);
        t.record_down_chunk(Some("s1"), 200);
        let snap = t.snapshot();
        let g = &snap["global"];
        assert_eq!(g["total_up"], 1510u64);
        assert_eq!(g["total_down"], 500u64);
        let s = &snap["sessions"]["s1"];
        assert_eq!(s["total_up"], 1500u64);
        assert_eq!(s["total_down"], 500u64);
        // 速率：同一秒记账，窗口均值 = 总量/窗口秒数
        assert_eq!(s["rates"]["up"]["1s"], 1500.0);
        assert_eq!(s["rates"]["up"]["5s"], 300.0);
        assert_eq!(s["rates"]["down"]["1s"], 500.0);
        assert_eq!(s["rates"]["down"]["60s"], 500.0 / 60.0);
    }

    /// session 超限淘汰：最久不活跃的被逐，活跃的保留。
    #[test]
    fn traffic_evicts_stale_sessions() {
        let t = Traffic::new();
        t.record_up(Some("old"), 1);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        t.record_up(Some("new"), 1);
        // 填满到上限（期间 old → new 按插入序被逐个淘汰）
        for i in 0..MAX_SESSIONS {
            t.record_up(Some(&format!("fill-{i}")), 1);
        }
        // 重新活跃 new：在淘汰后重新插入（contains_key 命中则只更新，不触发淘汰）
        t.record_up(Some("new"), 1);
        let snap = t.snapshot();
        let sessions = snap["sessions"].as_object().unwrap();
        assert!(sessions.len() <= MAX_SESSIONS, "不应超上限");
        assert!(sessions.contains_key("new"), "重新活跃的 session 应存在");
        assert!(!sessions.contains_key("old"), "最久不活跃应先被淘汰");
    }

    #[test]
    fn traffic_snapshot_shape() {
        let t = Traffic::new();
        t.record_up(Some("s"), 10);
        let snap = t.snapshot();
        assert_eq!(snap["window_seconds"], 60);
        assert!(snap["global"]["rates"]["up"]["1s"].is_f64());
        assert!(snap["global"]["rates"]["down"]["5s"].is_f64());
        assert!(snap["global"]["rates"]["down"]["60s"].is_f64());
        assert!(snap["global"].get("instant").is_none());
    }
}
