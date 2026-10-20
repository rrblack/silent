use anyhow::Context;
use silent_core::proto::IceConfig;
use silent_relay::RelayConfig;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let addr = std::env::var("SILENT_RELAY_ADDR").unwrap_or_else(|_| "127.0.0.1:8787".into());
    let ice = match std::env::var("SILENT_ICE_JSON") {
        Ok(json) => serde_json::from_str(&json).context("SILENT_ICE_JSON is not a valid IceConfig")?,
        Err(_) => IceConfig::default(),
    };

    let listener = tokio::net::TcpListener::bind(&addr).await.with_context(|| format!("bind {addr}"))?;
    tracing::info!("silent-relay listening on {}", listener.local_addr()?);
    silent_relay::serve(listener, RelayConfig { ice }).await?;
    Ok(())
}
