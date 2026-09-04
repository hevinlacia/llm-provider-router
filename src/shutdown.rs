//! 优雅退出：SIGTERM/SIGINT 后停止接新连接，存量请求/SSE 流跑完再退。
//!
//! systemd stop（idle 下线 / 手动停槽 / 重启）都会先发 SIGTERM；进程没有优雅退出时，
//! 存量流会被立即切断——这正是"部署对 agent 无感"要消灭的中断源。
//! 看门狗 85s：刻意落在 systemd 默认 TimeoutStopSec(90s) 之内，由进程自己先退出，
//! 避免 SIGKILL 硬切（超时仍卡死的连接只能放弃）。

use std::time::Duration;

const DRAIN_TIMEOUT_SECS: u64 = 85;

/// 阻塞直到收到首个退出信号；返回后调用方应尽快完成 serve 的 graceful drain。
pub async fn wait_for_shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            // 信号注册失败：永不触发该分支，仅依赖 ctrl_c
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    eprintln!(
        "shutdown signal received: draining in-flight connections (max {DRAIN_TIMEOUT_SECS}s)..."
    );
    // 看门狗：drain 卡死时强制退出，宁可放弃长尾流也不被 SIGKILL 硬切
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(DRAIN_TIMEOUT_SECS)).await;
        eprintln!("drain timeout ({DRAIN_TIMEOUT_SECS}s) reached, forcing exit");
        std::process::exit(0);
    });
}
