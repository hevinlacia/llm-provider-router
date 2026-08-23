//! 共享运行时状态：AppState 组装 HTTP 依赖、RouterState、search 池。
//!
//! 供 `routes/*`（handler 薄层）与 `features/chat`（代理主链路）共同访问；
//! 字段 `pub(crate)` 以便同 crate 内子模块读取。

use crate::config::Settings;
use crate::features::router::RouterState;
use crate::search::SearchPool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone)]
pub struct AppState {
    pub(crate) settings: Settings,
    pub(crate) state: Arc<Mutex<RouterState>>,
    pub(crate) client: reqwest::Client,
    pub(crate) search_pool: Arc<Mutex<SearchPool>>,
}

impl AppState {
    pub fn new(settings: Settings) -> anyhow::Result<Self> {
        let timeout = std::time::Duration::from_secs_f64(settings.request_timeout_seconds);
        // opencode-go (zen/go) 上游用 Cloudflare 拦截非浏览器 UA（error 1010），
        // 全局使用浏览器 UA 以兼容该上游；OpenAI 兼容 API 不校验 UA，无副作用。
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .pool_idle_timeout(Duration::from_secs(30))
            .tcp_keepalive(Duration::from_secs(30))
            .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36")
            .build()?;
        let state = Arc::new(Mutex::new(RouterState::new(settings.clone())?));
        let search_pool = Arc::new(Mutex::new(SearchPool::new(&settings.search_providers_path)));
        Ok(Self {
            settings,
            state,
            client,
            search_pool,
        })
    }
}
