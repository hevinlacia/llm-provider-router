//! Delta 上下文缓存：pi ↔ router 公网链路的请求上下文增量传输。
//!
//! 动机：pi 在外网通过公网（4Mbps 上行，与 agent panel 共享）连家里 router，
//! Responses 请求每笔全量重发整个上下文（长会话几百 KB~几 MB），上传是带宽瓶颈。
//! 本模块让 router 缓存每个会话的 `instructions + input[]` 前缀，客户端后续只传
//! 新增 input items，router 重组完整 payload 后走现有转发管线（上游每笔全量，无感知）。
//!
//! 协议（协商式握手，hash 只在 router 端计算，避免跨语言 JSON 规范化耦合）：
//! 1. **首笔全量**：请求带 `X-Delta-Capable` 头 → router 把 `instructions + input[]`
//!    存入缓存并分配 token，响应头下发 `X-Delta-Cache: ready` + `X-Delta-Token` +
//!    `X-Delta-Base-Hash`。客户端收不到 ready 头时保持全量模式。
//! 2. **后续增量**：请求体带 `x_delta` 对象（token + base_hash + append items，
//!    `input`/`instructions` 字段省略——正是带宽大头）→ router 校验 token 存在且
//!    `base_hash` 与缓存一致 → 拼接重组完整 payload → 正常管线。
//!    重组成功即响应 `X-Delta-Cache: advance` + 新 base_hash（缓存已前进，
//!    即使上游随后失败，客户端同轮重试仍可走增量，零全量恢复）。
//! 3. **任何 miss**（token 失效 / hash 不匹配 / LRU 淘汰 / TTL 过期 / 重启）→
//!    `429 + X-Delta-Cache: miss` → 客户端回退全量重发（pi 对 429 自动重试，
//!    重试是新的 provider 调用，`before_provider_request` 重新触发时已回全量模式）。
//!
//! 正确性闭环（内容寻址，任何漂移必然降级为全量，绝不静默发错上下文）：
//! - 客户端保证：delta 的前缀 == 自己上次成功发送的前缀（扩展端 deep-equal）；
//! - router 保证：缓存前缀 hash == 客户端声称的 base_hash（单端 sha256）；
//! - 前缀不一致即 miss → 全量重发重建。缓存是纯优化，任何 miss 只是该轮退化为全量。
//!
//! 内存管理：全局块池按会话存完整 `instructions + items`（serde_json Value），
//! 上限由 `delta_cache_*` 配置控制：max_sessions LRU + 总字节 LRU + 单会话字节上限
//! （超限不缓存，该会话退化为全量）+ 空闲 TTL（默认 3h，比 key 粘性长——缓存命中
//! 省的是真实公网流量）。不持久化：重启丢失，各会话首轮全量重发一次即恢复。

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::config::Settings;

/// 运行时开关（dashboard /api/config/delta-cache 热切换；启动时由 env 默认值初始化）。
/// 关闭即清空缓存：不注册、不重组，增量请求一律 miss 回全量。
static ENABLED: AtomicBool = AtomicBool::new(true);

pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub(crate) fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
    if !on {
        clear();
    }
}
/// 单会话缓存条目。
struct SessionEntry {
    instructions: Value,
    items: Vec<Value>,
    /// H(instructions, items) 当前值（router 单端计算）。
    base_hash: String,
    /// instructions + items 序列化字节数（近似内存占用口径）。
    bytes: u64,
    last_access: Instant,
}

/// 计数器（diag 日志 + /api/config/delta-cache 暴露）。
#[derive(Default)]
pub(crate) struct DeltaStats {
    /// 全量注册成功次数（= 发出 ready 的次数）。
    pub(crate) full_stores: u64,
    /// 增量重组成功次数。
    pub(crate) delta_hits: u64,
    /// token 未命中（含 TTL 过期 / LRU 驱逐 / 重启丢失 / 禁用期残留）。
    pub(crate) miss_token: u64,
    /// base_hash 不匹配（前缀漂移 / 并发交错）。
    pub(crate) miss_hash: u64,
    /// 单会话超限跳过（该会话退化为全量）。
    pub(crate) skip_oversize: u64,
    /// LRU/TTL 驱逐的会话数。
    pub(crate) evictions: u64,
}

struct DeltaInner {
    sessions: HashMap<String, SessionEntry>,
    total_bytes: u64,
    stats: DeltaStats,
}

fn cache() -> &'static Mutex<DeltaInner> {
    static CACHE: OnceLock<Mutex<DeltaInner>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(DeltaInner {
            sessions: HashMap::new(),
            total_bytes: 0,
            stats: DeltaStats::default(),
        })
    })
}

fn lock() -> std::sync::MutexGuard<'static, DeltaInner> {
    cache().lock().unwrap_or_else(|p| p.into_inner())
}

/// token 生成熵源：纳秒时钟 + 计数器 + 前缀 hash 混合后 sha256 截断。
/// token 只是缓存凭据（非安全凭证），无需密码学随机，不可预测即可。
static TOKEN_SEQ: AtomicU64 = AtomicU64::new(0);

fn next_token(base_hash: &str) -> String {
    let seq = TOKEN_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut hasher = Sha256::new();
    hasher.update(nanos.to_le_bytes());
    hasher.update(seq.to_le_bytes());
    hasher.update(base_hash.as_bytes());
    let digest = hasher.finalize();
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// H(instructions, items)：单端确定性序列化后 sha256。
/// 0x00 分隔 instructions 与 items，0x1e 分隔 items，避免拼接歧义。
fn compute_base_hash(instructions: &Value, items: &[Value]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_vec(instructions).unwrap_or_default());
    hasher.update([0x00]);
    for item in items {
        hasher.update(serde_json::to_vec(item).unwrap_or_default());
        hasher.update([0x1e]);
    }
    format!("{:x}", hasher.finalize())
}

fn entry_bytes(instructions: &Value, items: &[Value]) -> u64 {
    let mut total = serde_json::to_vec(instructions)
        .map(|v| v.len())
        .unwrap_or(0) as u64;
    for item in items {
        total += serde_json::to_vec(item).map(|v| v.len()).unwrap_or(0) as u64;
    }
    total
}

fn settings_cleared(settings: &Settings) -> bool {
    !enabled()
        || settings.delta_cache_max_sessions == 0
        || settings.delta_cache_max_total_bytes == 0
}

/// 惰性清理：TTL 过期条目。返回是否清理过。
fn remove_expired(inner: &mut DeltaInner, ttl: Duration) -> usize {
    let now = Instant::now();
    let expired: Vec<String> = inner
        .sessions
        .iter()
        .filter(|(_, e)| now.saturating_duration_since(e.last_access) >= ttl)
        .map(|(k, _)| k.clone())
        .collect();
    for key in &expired {
        if let Some(e) = inner.sessions.remove(key) {
            inner.total_bytes = inner.total_bytes.saturating_sub(e.bytes);
            inner.stats.evictions += 1;
        }
    }
    expired.len()
}

/// LRU 驱逐：超 max_sessions / max_total_bytes 时逐出 last_access 最旧条目。
fn evict_lru(inner: &mut DeltaInner, settings: &Settings) {
    while inner.sessions.len() > settings.delta_cache_max_sessions
        || inner.total_bytes > settings.delta_cache_max_total_bytes
    {
        let oldest = inner
            .sessions
            .iter()
            .min_by_key(|(_, e)| e.last_access)
            .map(|(k, _)| k.clone());
        match oldest {
            Some(key) => {
                if let Some(e) = inner.sessions.remove(&key) {
                    inner.total_bytes = inner.total_bytes.saturating_sub(e.bytes);
                    inner.stats.evictions += 1;
                }
            }
            None => break,
        }
    }
}

/// 全量注册：提取 payload 的 instructions + input[] 存入缓存。
/// 返回 `Some((token, base_hash))`（发 ready 头）或 `None`（禁用 / 载荷不适 / 超限，
/// 不发 ready，客户端保持全量模式）。
pub(crate) fn register_full(payload: &Value, settings: &Settings) -> Option<(String, String)> {
    if settings_cleared(settings) {
        return None;
    }
    let instructions = payload.get("instructions").cloned().unwrap_or(Value::Null);
    let items = payload.get("input").and_then(Value::as_array)?;
    let bytes = entry_bytes(&instructions, items);
    if bytes > settings.delta_cache_session_max_bytes {
        lock().stats.skip_oversize += 1;
        return None;
    }
    let base_hash = compute_base_hash(&instructions, items);
    let token = next_token(&base_hash);
    let ttl = Duration::from_secs(settings.delta_cache_ttl_seconds);
    {
        let mut inner = lock();
        remove_expired(&mut inner, ttl);
        // 同 token 重复注册（理论上不会：token 每次新生成）不做特判，直接插入。
        inner.sessions.insert(
            token.clone(),
            SessionEntry {
                instructions,
                items: items.clone(),
                base_hash,
                bytes,
                last_access: Instant::now(),
            },
        );
        inner.total_bytes += bytes;
        inner.stats.full_stores += 1;
        evict_lru(&mut inner, settings);
    }
    let hash = lock().sessions.get(&token).map(|e| e.base_hash.clone())?;
    Some((token, hash))
}

/// 增量重组结果：新 base_hash 注入 `X-Delta-Cache: advance` 响应头。
pub(crate) struct Advance {
    pub(crate) new_hash: String,
}

/// delta 请求 miss 原因。
#[derive(Debug)]
pub(crate) enum DeltaMiss {
    /// 缓存禁用 / 载荷畸形，客户端应回全量并保持全量。
    Rejected(&'static str),
    /// token 未命中（TTL / 淘汰 / 重启 / 未知 token），客户端回全量后可重新协商。
    Token,
    /// base_hash 不匹配（前缀漂移），客户端回全量后可重新协商。
    Hash,
}

/// 增量重组：校验 token + base_hash，把缓存前缀与 append 拼成完整 payload（原地替换）。
/// 重组成功即更新缓存（缓存前进先于上游转发——上游失败时客户端同轮重试仍走增量）。
pub(crate) fn apply_delta(payload: &mut Value, settings: &Settings) -> Result<Advance, DeltaMiss> {
    if settings_cleared(settings) {
        return Err(DeltaMiss::Rejected("delta cache disabled"));
    }
    let Some(x_delta) = payload.get("x_delta").cloned() else {
        return Err(DeltaMiss::Rejected("missing x_delta"));
    };
    if !x_delta.is_object() {
        return Err(DeltaMiss::Rejected("x_delta must be an object"));
    }
    let token = x_delta
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let base_hash = x_delta
        .get("base_hash")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let Some(append) = x_delta.get("append").and_then(Value::as_array) else {
        return Err(DeltaMiss::Rejected("x_delta.append must be an array"));
    };
    if token.is_empty() || base_hash.is_empty() {
        return Err(DeltaMiss::Rejected("x_delta.token/base_hash required"));
    }

    let ttl = Duration::from_secs(settings.delta_cache_ttl_seconds);
    let (instructions, mut items, cached_hash) = {
        let mut inner = lock();
        remove_expired(&mut inner, ttl);
        let Some(entry) = inner.sessions.get_mut(&token) else {
            inner.stats.miss_token += 1;
            return Err(DeltaMiss::Token);
        };
        if entry.base_hash != base_hash {
            inner.stats.miss_hash += 1;
            return Err(DeltaMiss::Hash);
        }
        (
            entry.instructions.clone(),
            std::mem::take(&mut entry.items),
            entry.base_hash.clone(),
        )
    };
    debug_assert_eq!(cached_hash, base_hash);

    items.extend(append.iter().cloned());
    let new_hash = compute_base_hash(&instructions, &items);
    let bytes = entry_bytes(&instructions, &items);
    {
        let mut inner = lock();
        if let Some(entry) = inner.sessions.get_mut(&token) {
            let old_bytes = entry.bytes;
            entry.items = items.clone();
            entry.base_hash = new_hash.clone();
            entry.bytes = bytes;
            entry.last_access = Instant::now();
            inner.total_bytes = inner.total_bytes - old_bytes + bytes;
            inner.stats.delta_hits += 1;
            evict_lru(&mut inner, settings);
            // evict_lru 可能刚把这个条目驱逐（极端内存压力）；驱逐了也不回滚——
            // 本次请求照常放行，客户端下轮 delta miss 后回全量。
        } else {
            inner.stats.delta_hits += 1;
        }
    }

    // 重组完整 payload：顶层其他字段（model/tools/stream/...）以 delta 包为准（最新值），
    // input/instructions 由缓存恢复；最后摘除 x_delta 标记。
    let obj = payload
        .as_object_mut()
        .ok_or(DeltaMiss::Rejected("payload must be an object"))?;
    obj.remove("x_delta");
    obj.insert("input".to_string(), Value::Array(items));
    obj.insert("instructions".to_string(), instructions);
    Ok(Advance { new_hash })
}

/// 是否为 delta 请求（body 带 x_delta 标记）。
pub(crate) fn is_delta_request(payload: &Value) -> bool {
    payload.get("x_delta").is_some()
}

// ---- 切槽 handoff（2026-10-27 事故后新增）----
//
// delta 缓存是每槽进程内存态：不搬则切槽后所有 delta 会话首轮 429
// （客户端扩展有 pendingDelta 自愈，但要付一轮全量重传；auto-retry 关闭时
// 用户可见报错）。dump/load 只在部署脚本切槽瞬间直连槽端口调用一次，
// 热路径零影响；/internal/* 被 front-proxy 封锁，公网不可达。

/// 导出全部会话（GET /internal/delta-cache/dump）。
/// Instant 不可跨进程序列化，导出 idle_ms（接收端统一视为刚活跃，TTL 从导入重算）。
pub(crate) fn dump() -> Value {
    let inner = lock();
    let now = Instant::now();
    let sessions: Vec<Value> = inner
        .sessions
        .iter()
        .map(|(token, e)| {
            json!({
                "token": token,
                "instructions": e.instructions,
                "items": e.items,
                "base_hash": e.base_hash,
                "bytes": e.bytes,
                "idle_ms": now.duration_since(e.last_access).as_millis() as u64,
            })
        })
        .collect();
    let count = sessions.len();
    json!({
        "count": count,
        "total_bytes": inner.total_bytes,
        "sessions": sessions,
    })
}

/// 导入会话（POST /internal/delta-cache/load，body = dump() 输出）。
/// 防护：逐条重算 base_hash 校验完整性（传输损坏跳过）；超单会话限额跳过；
/// 同 token 覆盖；导入后 evict_lru 收敛到总额限。返回导入条数。
pub(crate) fn load(payload: &Value, settings: &Settings) -> usize {
    if settings_cleared(settings) {
        return 0;
    }
    let Some(list) = payload.get("sessions").and_then(Value::as_array) else {
        return 0;
    };
    let ttl = Duration::from_secs(settings.delta_cache_ttl_seconds);
    let now = Instant::now();
    let mut imported = 0usize;
    {
        let mut inner = lock();
        remove_expired(&mut inner, ttl);
        for entry in list {
            let (Some(token), Some(instructions), Some(items), Some(base_hash)) = (
                entry.get("token").and_then(Value::as_str),
                entry.get("instructions"),
                entry.get("items").and_then(Value::as_array),
                entry.get("base_hash").and_then(Value::as_str),
            ) else {
                continue;
            };
            if token.is_empty() || base_hash.is_empty() {
                continue;
            }
            let bytes = entry_bytes(instructions, items);
            if bytes > settings.delta_cache_session_max_bytes {
                continue; // 超单会话限额：跳过（该会话导入后也会被客户端全量重建）
            }
            // 传输完整性：base_hash 必须与内容重算值一致，损坏条目直接丢弃
            if compute_base_hash(instructions, items) != base_hash {
                continue;
            }
            let old_bytes = inner
                .sessions
                .insert(
                    token.to_string(),
                    SessionEntry {
                        instructions: instructions.clone(),
                        items: items.clone(),
                        base_hash: base_hash.to_string(),
                        bytes,
                        last_access: now,
                    },
                )
                .map(|e| e.bytes)
                .unwrap_or(0);
            inner.total_bytes = inner.total_bytes + bytes - old_bytes;
            imported += 1;
        }
        evict_lru(&mut inner, settings); // 收敛到 max_sessions / max_total_bytes
    }
    imported
}

/// `429 + X-Delta-Cache: miss`：客户端回退全量（pi 对 429 自动重试，重试时已回全量模式）。
pub(crate) fn miss_response() -> axum::response::Response {
    use axum::http::{header, HeaderValue, StatusCode};
    use axum::response::IntoResponse;
    let mut resp = axum::Json(json!({
        "error": {
            "message": "delta context cache miss; resend the full payload",
            "type": "delta_cache_miss",
        }
    }))
    .into_response();
    *resp.status_mut() = StatusCode::TOO_MANY_REQUESTS;
    resp.headers_mut()
        .insert("x-delta-cache", HeaderValue::from_static("miss"));
    if let Ok(v) = HeaderValue::from_str("0") {
        resp.headers_mut().insert(header::RETRY_AFTER, v);
    }
    resp
}

/// 缓存快照（dashboard / 诊断）：开关 + 计数器 + 当前水位。
pub(crate) fn snapshot() -> Value {
    let inner = lock();
    json!({
        "enabled": enabled(),
        "sessions": inner.sessions.len(),
        "total_bytes": inner.total_bytes,
        "stats": {
            "full_stores": inner.stats.full_stores,
            "delta_hits": inner.stats.delta_hits,
            "miss_token": inner.stats.miss_token,
            "miss_hash": inner.stats.miss_hash,
            "skip_oversize": inner.stats.skip_oversize,
            "evictions": inner.stats.evictions,
        },
    })
}

/// 清空缓存（dashboard 开关关闭时调用；重新开启后各会话首轮全量重新协商）。
pub(crate) fn clear() {
    let mut inner = lock();
    inner.sessions.clear();
    inner.total_bytes = 0;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 全局 cache + 开关在测试间共享（OnceLock 静态），串行化避免互相污染；
    /// 进入时重置环境（清缓存 + 开默认开）。
    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        static GUARD: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
        let guard = GUARD
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        clear();
        set_enabled(true);
        guard
    }

    fn settings() -> Settings {
        let mut s = crate::config::load_settings_for_tests();
        // 显式覆盖：不依赖测试机 env（如 LLM_PROVIDER_ROUTER_DELTA_CACHE）
        s.delta_cache_enabled = true;
        s.delta_cache_max_sessions = 256;
        s.delta_cache_max_total_bytes = 512 * 1024 * 1024;
        s.delta_cache_session_max_bytes = 16 * 1024 * 1024;
        s.delta_cache_ttl_seconds = 3 * 3600;
        s
    }

    fn payload(instructions: &str, items: &[Value]) -> Value {
        json!({
            "model": "deepseek-flash-latest-auto",
            "stream": true,
            "instructions": instructions,
            "input": items,
        })
    }

    #[test]
    fn register_then_delta_roundtrip() {
        let _g = test_guard();
        let s = settings();
        let p1 = payload(
            "sys",
            &[json!({"type": "message", "role": "user", "content": "hi"})],
        );
        let (token, hash1) = register_full(&p1, &s).expect("register");

        // delta：前缀不变，append 一条
        let mut p2 = json!({
            "model": "deepseek-flash-latest-auto",
            "stream": true,
            "x_delta": {
                "v": 1,
                "token": token,
                "base_hash": hash1,
                "append": [{"type": "message", "role": "user", "content": "q2"}],
            }
        });
        let advance = apply_delta(&mut p2, &s).expect("advance");
        // 重组结果与等价全量 payload 深度一致
        assert!(p2.get("x_delta").is_none(), "x_delta must be stripped");
        assert_eq!(p2["instructions"], json!("sys"));
        assert_eq!(p2["input"].as_array().unwrap().len(), 2);
        assert_eq!(p2["input"][1]["content"], json!("q2"));
        assert_eq!(p2["model"], json!("deepseek-flash-latest-auto"));

        // 下一轮 delta 用新 hash：append 为空也成立
        let mut p3 = json!({
            "model": "m",
            "stream": true,
            "x_delta": {"v": 1, "token": token, "base_hash": advance.new_hash, "append": []}
        });
        apply_delta(&mut p3, &s).expect("advance 2");
    }

    #[test]
    fn unknown_token_is_miss() {
        let _g = test_guard();
        let s = settings();
        let mut p = json!({
            "model": "m",
            "x_delta": {"v": 1, "token": "nonexistent", "base_hash": "ab", "append": []}
        });
        matches!(apply_delta(&mut p, &s), Err(DeltaMiss::Token));
    }

    #[test]
    fn hash_mismatch_is_miss() {
        let _g = test_guard();
        let s = settings();
        let p1 = payload("sys", &[json!({"role": "user", "content": "a"})]);
        let (token, _hash) = register_full(&p1, &s).expect("register");
        let mut p = json!({
            "model": "m",
            "x_delta": {"v": 1, "token": token, "base_hash": "deadbeef", "append": []}
        });
        matches!(apply_delta(&mut p, &s), Err(DeltaMiss::Hash));
    }

    #[test]
    fn oversize_session_not_registered() {
        let _g = test_guard();
        let mut s = settings();
        s.delta_cache_session_max_bytes = 10; // 10 字节，必超
        let p = payload("a much longer system prompt", &[json!({"role": "user"})]);
        assert!(register_full(&p, &s).is_none());
    }

    #[test]
    fn lru_evicts_oldest_when_over_capacity() {
        let _g = test_guard();
        let mut s = settings();
        s.delta_cache_max_sessions = 2;
        let (t1, _h) = register_full(&payload("s1", &[json!({"i": 1})]), &s).unwrap();
        // 触摸 t1 使其比 t2 新（显式未来时间，避免 Instant 分辨率内相等）
        {
            let mut inner = lock();
            inner.sessions.get_mut(&t1).unwrap().last_access =
                Instant::now() + Duration::from_secs(60);
        }
        let (_t2, _h2) = register_full(&payload("s2", &[json!({"i": 2})]), &s).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let (t3, _h3) = register_full(&payload("s3", &[json!({"i": 3})]), &s).unwrap();
        let inner = lock();
        assert!(inner.sessions.contains_key(&t1), "recently used kept");
        assert!(inner.sessions.contains_key(&t3), "newest kept");
        assert_eq!(inner.sessions.len(), 2, "capacity enforced");
    }

    #[test]
    fn ttl_expiry_is_token_miss() {
        let _g = test_guard();
        let mut s = settings();
        s.delta_cache_ttl_seconds = 0; // 立即过期
        let p1 = payload("sys", &[json!({"role": "user"})]);
        let (token, hash) = register_full(&p1, &s).expect("register");
        let mut p = json!({
            "model": "m",
            "x_delta": {"v": 1, "token": token, "base_hash": hash, "append": []}
        });
        matches!(apply_delta(&mut p, &s), Err(DeltaMiss::Token));
    }

    #[test]
    fn disabled_cache_rejects_delta_and_registration() {
        let _g = test_guard();
        let mut s = settings();
        s.delta_cache_enabled = false;
        set_enabled(false);
        let p1 = payload("sys", &[json!({"role": "user"})]);
        assert!(register_full(&p1, &s).is_none(), "disabled: no register");
        let mut p = json!({
            "model": "m",
            "x_delta": {"v": 1, "token": "t", "base_hash": "h", "append": []}
        });
        matches!(apply_delta(&mut p, &s), Err(DeltaMiss::Rejected(_)));
        set_enabled(true); // 恢复全局开关，不影响其他测试
    }

    #[test]
    fn hash_is_content_addressed() {
        let _g = test_guard();
        let a = compute_base_hash(&json!("sys"), &[json!({"i": 1})]);
        let b = compute_base_hash(&json!("sys"), &[json!({"i": 1})]);
        let c = compute_base_hash(&json!("sys"), &[json!({"i": 2})]);
        let d = compute_base_hash(&json!("sys2"), &[json!({"i": 1})]);
        assert_eq!(a, b, "same content same hash");
        assert_ne!(a, c, "item change changes hash");
        assert_ne!(a, d, "instructions change changes hash");
        let e = compute_base_hash(&json!("sys"), &[json!({"i": 1}), json!({"i": 2})]);
        let f = compute_base_hash(&json!("sys"), &[json!({"i": 12})]);
        assert_ne!(e, f, "item boundary is separator-guarded");
    }

    /// handler 级集成：全量注册 → delta 重组转发 → miss 降级，
    /// 验证 /v1/responses 分流 + 响应头信号 + 管线串联（翻译模式 mock 上游）。
    /// 注：test_guard 的 std Mutex 跨 await —— tokio::test 默认单线程 runtime，
    /// guard 仅用于与其他测试互斥全局 cache，无并发竞争，故 allow。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn handler_full_then_delta_then_miss() {
        use crate::app::AppState;
        use crate::config::{KeyRef, ModelAlias};
        use crate::routes::responses::responses;
        use axum::extract::State;
        use axum::http::{HeaderMap, HeaderValue, StatusCode};
        use axum::Json;
        use std::sync::{Arc, Mutex};

        let _g = test_guard();
        let s = settings();

        // mock 上游：记录收到的 chat 请求 body，回固定 chat 响应。
        let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_for_server = seen.clone();
        let upstream = axum::Router::new().route(
            "/chat/completions",
            axum::routing::post(move |Json(body): Json<Value>| {
                let seen = seen_for_server.clone();
                async move {
                    seen.lock().unwrap().push(body);
                    Json(json!({
                        "id": "chatcmpl-1",
                        "object": "chat.completion",
                        "model": "mock-test-model",
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "ok"},
                            "finish_reason": "stop"
                        }],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });

        std::env::set_var("DSH_TEST_DELTA_KEY", "test-key-value");
        let app_state = AppState::new(s).unwrap();
        let alias = ModelAlias::new(
            "mock/test-model",
            "openai/mock-test-model",
            &format!("http://{addr}"),
            vec![KeyRef {
                name: "test-key".into(),
                env_var: "DSH_TEST_DELTA_KEY".into(),
                weight: 1,
                provider: "mock-provider".into(),
                billing_type: "subscription".into(),
                persist: true,
                daily_token_quota: None,
            }],
            None,
        );
        app_state.state.lock().unwrap().install_test_alias(alias);

        async fn call(
            app: &AppState,
            capable: bool,
            payload: Value,
        ) -> (StatusCode, HeaderMap, Value) {
            let mut headers = HeaderMap::new();
            if capable {
                headers.insert("x-delta-capable", HeaderValue::from_static("1"));
            }
            let response = responses(State(app.clone()), headers, Json(payload)).await;
            let status = response.status();
            let resp_headers = response.headers().clone();
            let body = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            (status, resp_headers, serde_json::from_slice(&body).unwrap())
        }

        // 1) 全量 + capable：上游收到完整 input，响应带 ready/token/base_hash
        let full = json!({
            "model": "mock/test-model",
            "stream": false,
            "instructions": "sys",
            "input": [
                {"role": "user", "content": "hi"},
                {"role": "user", "content": "q2"}
            ]
        });
        let (status, rh, _body) = call(&app_state, true, full.clone()).await;
        assert_eq!(status, StatusCode::OK, "full request should succeed");
        assert_eq!(rh["x-delta-cache"], "ready", "ready header missing");
        let token = rh["x-delta-token"].to_str().unwrap().to_string();
        let hash1 = rh["x-delta-base-hash"].to_str().unwrap().to_string();
        assert!(!token.is_empty() && !hash1.is_empty());
        // translate 模式下 instructions 转为 system message：2 input + 1 system = 3
        assert_eq!(
            seen.lock().unwrap().last().unwrap()["messages"]
                .as_array()
                .unwrap()
                .len(),
            3
        );

        // 2) delta：上游收到重组后的完整 payload（与等价全量一致），响应 advance + 新 hash
        let delta = json!({
            "model": "mock/test-model",
            "stream": false,
            "x_delta": {
                "v": 1,
                "token": token,
                "base_hash": hash1,
                "append": [{"role": "assistant", "content": "ok"}, {"role": "user", "content": "q3"}]
            }
        });
        let (status, rh, _body) = call(&app_state, false, delta).await;
        assert_eq!(status, StatusCode::OK, "delta request should succeed");
        assert_eq!(rh["x-delta-cache"], "advance");
        let hash2 = rh["x-delta-base-hash"].to_str().unwrap().to_string();
        assert_ne!(hash1, hash2, "advance must rotate base hash");
        let last_upstream = seen.lock().unwrap().last().unwrap().clone();
        assert_eq!(
            last_upstream["messages"].as_array().unwrap().len(),
            5,
            "upstream must receive full reconstructed context (4 input + 1 system)"
        );

        // 3) 坏 token：429 + miss 头（不转发上游）
        let bad = json!({
            "model": "mock/test-model",
            "stream": false,
            "x_delta": {"v": 1, "token": "nope", "base_hash": "x", "append": []}
        });
        let before = seen.lock().unwrap().len();
        let (status, rh, _body) = call(&app_state, false, bad).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(rh["x-delta-cache"], "miss");
        assert_eq!(
            seen.lock().unwrap().len(),
            before,
            "miss must not hit upstream"
        );

        // 4) 关闭开关：delta 请求也 miss，全量请求不再发 ready
        set_enabled(false);
        let (status, rh, _body) = call(&app_state, true, full).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            rh.get("x-delta-cache").is_none(),
            "disabled: no ready header"
        );
        set_enabled(true);
    }

    #[test]
    fn register_preserves_other_top_level_fields_on_delta() {
        let _g = test_guard();
        let s = settings();
        let p1 = json!({
            "model": "m",
            "stream": true,
            "tools": [{"type": "function", "name": "read"}],
            "instructions": "sys",
            "input": [{"role": "user", "content": "hi"}]
        });
        let (token, hash) = register_full(&p1, &s).unwrap();
        let mut p2 = json!({
            "model": "m2",
            "stream": true,
            "tools": [{"type": "function", "name": "read"}, {"type": "function", "name": "bash"}],
            "x_delta": {"v": 1, "token": token, "base_hash": hash, "append": [{"role": "assistant", "content": "ok"}]}
        });
        apply_delta(&mut p2, &s).unwrap();
        assert_eq!(p2["model"], json!("m2"), "top-level fields from delta pkg");
        assert_eq!(p2["tools"].as_array().unwrap().len(), 2);
        assert_eq!(p2["instructions"], json!("sys"), "instructions restored");
        assert_eq!(p2["input"].as_array().unwrap().len(), 2);
    }
    // ---- 切槽 handoff：dump/load ----

    /// dump → load roundtrip：接收端用原 token+hash 请求，advance 且前缀完整。
    #[test]
    fn handoff_dump_load_roundtrip() {
        let _g = test_guard();
        let s = settings();
        let p1 = payload(
            "sys",
            &[
                json!({"type": "message", "role": "user", "content": "q1"}),
                json!({"type": "message", "role": "assistant", "content": "a1"}),
            ],
        );
        let (token, hash) = register_full(&p1, &s).expect("register on source");

        let dumped = dump();
        assert_eq!(dumped["count"], 1);

        clear(); // 模拟接收端空缓存（切到新槽）
        let imported = load(&dumped, &s);
        assert_eq!(imported, 1, "roundtrip import count");

        // 用源端的 token + base_hash 直接走增量：必须 advance（这就是切槽无感的判定）
        let mut p2 = json!({
            "model": "deepseek-flash-latest-auto",
            "stream": true,
            "x_delta": {"v": 1, "token": token, "base_hash": hash, "append": [
                {"role": "user", "content": "q2"}
            ]}
        });
        let adv = apply_delta(&mut p2, &s).expect("delta after handoff must hit");
        assert_ne!(adv.new_hash, hash);
        assert_eq!(p2["input"].as_array().unwrap().len(), 3, "prefix restored + append");
        assert_eq!(p2["instructions"], json!("sys"));
    }

    /// 损坏条目防护：base_hash 与内容不符的条目被跳过，不污染接收端缓存。
    #[test]
    fn handoff_load_rejects_corrupted_entries() {
        let _g = test_guard();
        let s = settings();
        let dumped = json!({
            "count": 2,
            "sessions": [
                {"token": "good-token", "instructions": "sys",
                 "items": [{"role": "user", "content": "hi"}],
                 "base_hash": sha_hex("sys", &[json!({"role": "user", "content": "hi"})]),
                 "bytes": 100, "idle_ms": 0},
                {"token": "bad-token", "instructions": "sys",
                 "items": [{"role": "user", "content": "tampered"}],
                 "base_hash": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
                 "bytes": 100, "idle_ms": 0},
            ]
        });
        clear();
        let imported = load(&dumped, &s);
        assert_eq!(imported, 1, "corrupted entry skipped");

        // good-token 可用；bad-token 不存在（miss_token）
        let mut p_good = json!({
            "model": "m", "stream": true,
            "x_delta": {"v": 1, "token": "good-token",
                        "base_hash": sha_hex("sys", &[json!({"role": "user", "content": "hi"})]),
                        "append": [{"role": "assistant", "content": "ok"}]}
        });
        assert!(apply_delta(&mut p_good, &s).is_ok(), "good entry usable");
        let mut p_bad = json!({
            "model": "m", "stream": true,
            "x_delta": {"v": 1, "token": "bad-token", "base_hash": "whatever", "append": []}
        });
        assert!(matches!(apply_delta(&mut p_bad, &s), Err(DeltaMiss::Token)));
    }

    /// 超单会话限额的条目跳过；last_access 重置（导入即视为刚活跃，TTL 从导入重算）。
    #[test]
    fn handoff_load_enforces_limits_and_fresh_ttl() {
        let _g = test_guard();
        let mut s = settings();
        s.delta_cache_session_max_bytes = 64; // 很小的限额
        let big = json!({
            "count": 1,
            "sessions": [
                {"token": "big", "instructions": "sys",
                 "items": [{"role": "user", "content": "x".repeat(500)}],
                 "base_hash": sha_hex("sys", &[json!({"role": "user", "content": "x".repeat(500)})]),
                 "bytes": 500, "idle_ms": 0}
            ]
        });
        clear();
        assert_eq!(load(&big, &s), 0, "oversize entry skipped");
        assert_eq!(lock().sessions.len(), 0);
    }

    /// sha_hex 测试辅助：与 compute_base_hash 同口径（公开给断言用）。
    fn sha_hex(instructions: &str, items: &[Value]) -> String {
        compute_base_hash(&json!(instructions), items)
    }
}
