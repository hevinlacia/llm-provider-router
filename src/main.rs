mod config;
mod front_proxy;
mod json_config;
mod proxy;
mod router_state;
mod state_store;
mod usage_store;

use anyhow::Context;
use config::load_settings;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "backend".to_string());
    match mode.as_str() {
        "backend" | "router" => proxy::serve(load_settings()?).await,
        "front-proxy" | "proxy" => front_proxy::serve().await,
        "--help" | "-h" | "help" => {
            eprintln!("Usage: llm-provider-router [backend|front-proxy]");
            Ok(())
        }
        other => anyhow::bail!("unknown mode: {other}. Expected backend or front-proxy"),
    }
    .context("llm-provider-router failed")
}
