use anyhow::Context;
use silent_core::proto::IceConfig;
use silent_relay::RelayConfig;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    // All interfaces by default so phones and other computers on the same
    // network can use it. It only ever relays sealed messages.
    let addr = std::env::var("SILENT_RELAY_ADDR").unwrap_or_else(|_| "0.0.0.0:8787".into());
    let ice = match std::env::var("SILENT_ICE_JSON") {
        Ok(json) => {
            serde_json::from_str(&json).context("SILENT_ICE_JSON is not a valid IceConfig")?
        }
        Err(_) => IceConfig::default(),
    };

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    let local = listener.local_addr()?;
    tracing::info!("silent-relay listening on {local}");
    if local.ip().is_unspecified() {
        tracing::info!(
            "other devices on this network: set Settings → Relay server to http://<this computer's LAN IP>:{}",
            local.port()
        );
    }
    silent_relay::serve(listener, RelayConfig { ice }).await?;
    Ok(())
}
