//! 槽生命周期管理：非活跃槽无流量自动下线，活跃槽不在线自动拉起。
//!
//! front proxy（流量入口）是唯一管理槽生命周期的角色，backend 进程自身不感知槽位：
//! - 每次请求实际转发到某槽时 `touch()` 刷新该槽的最后流量时间；
//! - 后台循环定期检查：**非活跃槽**连续 `idle_shutdown_secs` 无流量 → `systemctl --user stop`
//!   下线；**活跃槽** health 不通且 `auto_heal` 开启 → `systemctl --user start` 拉起；
//! - 决策逻辑 `decide` 为纯函数，systemctl 副作用通过 `SystemCtl` trait 抽象，便于单测。

use reqwest::Client;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

/// 槽定义：与 front_proxy 的 configured_backends 一一对应。
#[derive(Clone, Debug)]
pub struct SlotSpec {
    pub slot: String,
    pub base_url: String,
    pub service_name: String,
}

/// 管理动作。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// 不执行任何动作。
    None,
    /// 拉起槽对应 systemd 服务（`systemctl --user start`）。
    Start,
    /// 下线槽对应 systemd 服务（`systemctl --user stop`）。
    Stop,
}

impl Action {
    pub fn as_str(&self) -> &'static str {
        match self {
            Action::None => "none",
            Action::Start => "start",
            Action::Stop => "stop",
        }
    }
}

/// systemctl 副作用抽象（生产 = 真实 systemd；测试 = fake 记录调用）。
pub trait SystemCtl: Send + Sync {
    fn invoke(&self, action: Action, service: &str);
}

/// 真实 systemd 调用：`systemctl --user <start|stop> <service>`。
pub struct RealSystemCtl;

impl SystemCtl for RealSystemCtl {
    fn invoke(&self, action: Action, service: &str) {
        let verb = match action {
            Action::Start => "start",
            Action::Stop => "stop",
            Action::None => return,
        };
        // 异步 spawn 真实命令，避免阻塞管理循环；失败静默（下次循环重试）。
        let service = service.to_string();
        let verb = verb.to_string();
        tokio::spawn(async move {
            let Ok(mut cmd) = tokio::process::Command::new("systemctl")
                .arg("--user")
                .arg(&verb)
                .arg(&service)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            else {
                return;
            };
            let _ = tokio::time::timeout(SYSTEMCTL_TIMEOUT, cmd.wait()).await;
        });
    }
}

const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(15);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const ACTION_COOLDOWN_SECS: u64 = 15;

/// 每槽在途请求数（front-proxy 转发成功响应时 +1，响应流结束/断开时 -1）。
/// idle 下线前必须等在途流清零，否则 `systemctl stop` 会切断存量流。
#[derive(Default)]
pub struct SlotInflight {
    counts: HashMap<String, AtomicI64>,
}

impl SlotInflight {
    pub fn new(slots: &[String]) -> Self {
        Self {
            counts: slots
                .iter()
                .map(|slot| (slot.clone(), AtomicI64::new(0)))
                .collect(),
        }
    }

    /// 登记一个在途请求；返回 guard，Drop（流结束/客户端断开）时自动递减。
    pub fn enter(self: &Arc<Self>, slot: &str) -> SlotInflightGuard {
        if let Some(count) = self.counts.get(slot) {
            count.fetch_add(1, Ordering::Relaxed);
        }
        SlotInflightGuard {
            tracker: Arc::clone(self),
            slot: slot.to_string(),
        }
    }

    pub fn get(&self, slot: &str) -> u64 {
        self.counts
            .get(slot)
            .map(|count| count.load(Ordering::Relaxed).max(0) as u64)
            .unwrap_or(0)
    }
}

/// 在途请求计数 guard：随响应流的生命周期存活，Drop 时递减。
pub struct SlotInflightGuard {
    tracker: Arc<SlotInflight>,
    slot: String,
}

impl Drop for SlotInflightGuard {
    fn drop(&mut self) {
        if let Some(count) = self.tracker.counts.get(&self.slot) {
            count.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

#[derive(Default)]
struct Inner {
    /// 每槽最后流量 unix 秒；无流量记录时用 first_seen 兜底计时。
    last_traffic: HashMap<String, u64>,
    /// 每槽首次被本进程观察到的时间（用于从未收到流量的槽也能被 idle 下线）。
    first_seen: HashMap<String, u64>,
    /// 每槽最近一次探测的健康状态。
    last_health: HashMap<String, bool>,
    /// 每槽最近一次执行的动作与时间（防抖，避免重复 start/stop）。
    last_action: HashMap<String, (Action, u64)>,
}

/// 槽生命周期管理器。`Arc` 共享给代理 handler 用于 `touch`，管理循环 `run` 独立 spawn。
pub struct SlotManager {
    specs: Arc<Vec<SlotSpec>>,
    client: Client,
    idle_shutdown_secs: u64,
    check_interval: Duration,
    auto_heal: bool,
    systemctl: Arc<dyn SystemCtl>,
    inflight: Arc<SlotInflight>,
    inner: Arc<Mutex<Inner>>,
}

impl SlotManager {
    pub fn new(
        specs: Vec<SlotSpec>,
        client: Client,
        idle_shutdown_secs: u64,
        check_interval: Duration,
        auto_heal: bool,
    ) -> Self {
        Self::with_systemctl(
            specs,
            client,
            idle_shutdown_secs,
            check_interval,
            auto_heal,
            Arc::new(RealSystemCtl),
        )
    }

    pub fn with_systemctl(
        specs: Vec<SlotSpec>,
        client: Client,
        idle_shutdown_secs: u64,
        check_interval: Duration,
        auto_heal: bool,
        systemctl: Arc<dyn SystemCtl>,
    ) -> Self {
        let inflight = Arc::new(SlotInflight::new(
            &specs
                .iter()
                .map(|spec| spec.slot.clone())
                .collect::<Vec<_>>(),
        ));
        Self {
            specs: Arc::new(specs),
            client,
            idle_shutdown_secs,
            check_interval,
            auto_heal,
            systemctl,
            inflight,
            inner: Arc::new(Mutex::new(Inner::default())),
        }
    }

    /// 在途计数器的共享句柄：front-proxy 用它登记/释放每个转发请求。
    pub fn inflight(&self) -> Arc<SlotInflight> {
        Arc::clone(&self.inflight)
    }

    /// 记录某槽收到一次流量（幂等，只刷新时间戳）。
    pub async fn touch(&self, slot: &str) {
        let mut inner = self.inner.lock().await;
        inner.last_traffic.insert(slot.to_string(), now_secs());
    }

    /// 管理循环入口：在 serve 中 spawn。`active_reader` 返回当前活跃槽名。
    pub async fn run(&self, active_reader: impl Fn() -> String + Send + Sync + 'static) {
        loop {
            tokio::time::sleep(self.check_interval).await;
            self.check_once(&active_reader()).await;
        }
    }

    /// 单轮检查（可测）。遍历所有槽：探测健康 → 决策 → 执行动作。
    pub async fn check_once(&self, active: &str) {
        for spec in self.specs.iter() {
            let healthy = self.probe(spec).await;
            let action = {
                let mut inner = self.inner.lock().await;
                let now = now_secs();
                if !inner.first_seen.contains_key(&spec.slot) {
                    inner.first_seen.insert(spec.slot.clone(), now);
                }
                inner.last_health.insert(spec.slot.clone(), healthy);
                let first = *inner.first_seen.get(&spec.slot).unwrap_or(&now);
                let last_traffic = inner.last_traffic.get(&spec.slot).copied().unwrap_or(first);
                let last_action = inner
                    .last_action
                    .get(&spec.slot)
                    .copied()
                    .unwrap_or((Action::None, 0));
                let action = decide(
                    &spec.slot,
                    active,
                    healthy,
                    last_traffic,
                    now,
                    self.idle_shutdown_secs,
                    self.auto_heal,
                    self.inflight.get(&spec.slot),
                    last_action,
                    ACTION_COOLDOWN_SECS,
                );
                if action != Action::None {
                    inner.last_action.insert(spec.slot.clone(), (action, now));
                }
                action
            };
            self.systemctl.invoke(action, &spec.service_name);
        }
    }

    /// 主动确保某槽在线（如 `POST /_proxy/active/{slot}` 切换后立即拉起，减少切换空白）。
    pub async fn ensure_running(&self, slot: &str) {
        let Some(spec) = self.specs.iter().find(|s| s.slot == slot) else {
            return;
        };
        self.systemctl.invoke(Action::Start, &spec.service_name);
    }

    async fn probe(&self, spec: &SlotSpec) -> bool {
        let url = format!("{}/health", spec.base_url.trim_end_matches('/'));
        match tokio::time::timeout(PROBE_TIMEOUT, self.client.get(&url).send()).await {
            Ok(Ok(resp)) => resp.status().is_success(),
            _ => false,
        }
    }

    /// 状态快照，供 `/_proxy/health` 展示。
    pub async fn snapshot(&self, active: &str) -> serde_json::Value {
        let inner = self.inner.lock().await;
        let now = now_secs();
        let slots: Vec<serde_json::Value> = self
            .specs
            .iter()
            .map(|spec| {
                let first = inner.first_seen.get(&spec.slot).copied().unwrap_or(now);
                let traffic_at = inner.last_traffic.get(&spec.slot).copied().unwrap_or(first);
                let (last_action, _) = inner
                    .last_action
                    .get(&spec.slot)
                    .copied()
                    .unwrap_or((Action::None, 0));
                json!({
                    "slot": spec.slot,
                    "base_url": spec.base_url,
                    "service": spec.service_name,
                    "healthy": inner.last_health.get(&spec.slot).copied().unwrap_or(false),
                    "is_active": spec.slot == active,
                    "last_traffic_at": traffic_at,
                    "idle_for_secs": now.saturating_sub(traffic_at),
                    "last_action": last_action.as_str(),
                })
            })
            .collect();
        json!({
            "idle_shutdown_secs": self.idle_shutdown_secs,
            "check_interval_secs": self.check_interval.as_secs(),
            "auto_heal": self.auto_heal,
            "slots": slots,
        })
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// 纯决策逻辑（便于单测）。
///
/// 规则：
/// - **活跃槽**：health 正常 → 无动作；health 异常且 `auto_heal` → `Start`（带冷却）。
/// - **非活跃槽**：health 正常、在途请求为 0、`idle_shutdown_secs>0` 且连续无流量超过阈值
///   → `Stop`（带冷却）；在途请求 >0 说明还有存量流没跑完，下线会切断连接，本轮跳过；
///   health 异常（已下线/启动中）→ 无动作（不重复 stop、不主动拉起非活跃槽）。
#[allow(clippy::too_many_arguments)]
pub fn decide(
    slot: &str,
    active: &str,
    healthy: bool,
    last_traffic: u64,
    now: u64,
    idle_shutdown_secs: u64,
    auto_heal: bool,
    inflight: u64,
    last_action: (Action, u64),
    cooldown_secs: u64,
) -> Action {
    let cooled = now.saturating_sub(last_action.1) >= cooldown_secs;
    if slot == active {
        if healthy {
            return Action::None;
        }
        return if auto_heal && cooled {
            Action::Start
        } else {
            Action::None
        };
    }
    // 非活跃槽
    if !healthy {
        return Action::None;
    }
    if idle_shutdown_secs == 0 {
        return Action::None;
    }
    // 还有在途流（切换前遗留的长流）→ 不下线，等下一轮
    if inflight > 0 {
        return Action::None;
    }
    let idle_secs = now.saturating_sub(last_traffic);
    if idle_secs >= idle_shutdown_secs && cooled {
        Action::Stop
    } else {
        Action::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_healthy_no_action() {
        assert_eq!(
            decide(
                "blue",
                "blue",
                true,
                0,
                1000,
                900,
                true,
                0,
                (Action::None, 0),
                15
            ),
            Action::None
        );
    }

    #[test]
    fn active_down_auto_heal_starts() {
        assert_eq!(
            decide(
                "blue",
                "blue",
                false,
                0,
                1000,
                900,
                true,
                0,
                (Action::None, 0),
                15
            ),
            Action::Start
        );
    }

    #[test]
    fn active_down_without_auto_heal_no_action() {
        assert_eq!(
            decide(
                "blue",
                "blue",
                false,
                0,
                900,
                900,
                false,
                0,
                (Action::None, 0),
                15
            ),
            Action::None
        );
    }

    #[test]
    fn inactive_idle_stops() {
        // green 非活跃，最后流量 500，now=1500 → idle 1000 >= 900 → Stop
        assert_eq!(
            decide(
                "green",
                "blue",
                true,
                500,
                1500,
                900,
                true,
                0,
                (Action::None, 0),
                15
            ),
            Action::Stop
        );
    }

    #[test]
    fn inactive_with_recent_traffic_no_stop() {
        assert_eq!(
            decide(
                "green",
                "blue",
                true,
                1499,
                1500,
                900,
                true,
                0,
                (Action::None, 0),
                15
            ),
            Action::None
        );
    }

    #[test]
    fn inactive_stop_respects_cooldown() {
        // 刚 stop 过（5s 前），冷却期内不再 stop
        assert_eq!(
            decide(
                "green",
                "blue",
                true,
                500,
                1500,
                900,
                true,
                0,
                (Action::Stop, 1495),
                15
            ),
            Action::None
        );
    }

    #[test]
    fn inactive_idle_disabled_never_stops() {
        assert_eq!(
            decide(
                "green",
                "blue",
                true,
                500,
                1500,
                0,
                true,
                0,
                (Action::None, 0),
                15
            ),
            Action::None
        );
    }

    #[test]
    fn inactive_down_no_repeat_stop() {
        assert_eq!(
            decide(
                "green",
                "blue",
                false,
                500,
                1500,
                900,
                true,
                0,
                (Action::None, 0),
                15
            ),
            Action::None
        );
    }

    #[test]
    fn never_touched_inactive_idles_from_first_seen() {
        // 从未有流量（last_traffic=first_seen=500），now=1500 → idle 1000 → Stop
        assert_eq!(
            decide(
                "green",
                "blue",
                true,
                500,
                1500,
                900,
                true,
                0,
                (Action::None, 0),
                15
            ),
            Action::Stop
        );
    }

    #[test]
    fn stopped_slot_that_is_active_again_gets_restarted() {
        // green 被切为活跃且当前下线（healthy=false）→ Start
        assert_eq!(
            decide(
                "green",
                "green",
                false,
                100,
                1500,
                900,
                true,
                0,
                (Action::None, 0),
                15
            ),
            Action::Start
        );
    }

    #[test]
    fn inactive_idle_but_inflight_no_stop() {
        // idle 已超阈值，但该槽还有存量流在跑 → 本轮不下线，避免切断连接
        assert_eq!(
            decide(
                "green",
                "blue",
                true,
                500,
                1500,
                900,
                true,
                2,
                (Action::None, 0),
                15
            ),
            Action::None
        );
    }
}
