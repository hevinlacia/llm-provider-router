//! 配置校验与文本工具（纯函数）。

pub(crate) fn sorted_join(mut values: Vec<String>) -> String {
    values.sort();
    values.dedup();
    values.join(", ")
}

/// 上游 `send()` 失败后是否重试（`attempt` 从 0 开始，最多尝试 2 次）。
///
/// 2026-11-05 全池瘫痪事故回归：原判定是
/// `(is_timeout || is_request) && !is_connect()`，把 `is_connect()` 排除在重试之外。
/// 但本机流量经路由器 OpenClash 透明代理出网，代理节点抖动时 hyper 报 connect 类
/// 错误——reqwest 的 Display 仍是「error sending request」（`Kind::Request`，故
/// `is_request()` 也为真），却被 `!is_connect()` 挡掉 → 一次瞬时失败就
/// `handle_connect_failure` 冻结唯一 key（30s 起指数退避，上限 960s），表现成
/// 「会话 A 用得好好的，会话 B 一请求，A 也报 no available upstream key」。
///
/// 现在一律重试一次：重试会新建连接（失败的连接不会回池），真死主机两次都失败
/// 才冻结；最坏代价仅多一次建连（由 connect_timeout 兜住上限）。
///
/// `is_connect` / `is_timeout` 保留入参仅作语义标注与日志用途，**不参与重试决策**。
pub(crate) fn should_retry_upstream_send(
    attempt: usize,
    is_connect: bool,
    is_timeout: bool,
) -> bool {
    let _ = (is_connect, is_timeout);
    attempt == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_class_error_is_retried_once() {
        // 代理节点抖动（connect 类）必须重试——2026-11-05 事故回归。
        assert!(should_retry_upstream_send(0, true, false));
        assert!(!should_retry_upstream_send(1, true, false));
    }

    #[test]
    fn timeout_and_request_errors_are_retried_once() {
        assert!(should_retry_upstream_send(0, false, true));
        assert!(!should_retry_upstream_send(1, false, true));
        assert!(should_retry_upstream_send(0, false, false));
        assert!(!should_retry_upstream_send(1, false, false));
    }
}
