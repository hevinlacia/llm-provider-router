mod app;
mod config;
mod config_v2;
mod diag;
mod features;
mod front_proxy;
mod json_config;
mod routes;
mod search;
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
        "backend" | "router" => routes::serve(load_settings()?).await,
        "front-proxy" | "proxy" => front_proxy::serve().await,
        "--help" | "-h" | "help" => {
            eprintln!("Usage: llm-provider-router [backend|front-proxy]");
            Ok(())
        }
        other => anyhow::bail!("unknown mode: {other}. Expected backend or front-proxy"),
    }
    .context("llm-provider-router failed")
}
