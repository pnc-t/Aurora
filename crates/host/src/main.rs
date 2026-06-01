//! DDD ホスト (操作される側) のエントリポイント。

use anyhow::Result;
use tracing_subscriber::EnvFilter;

mod net;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "ddd=info".into()))
        .init();

    tracing::info!("ddd-host starting (skeleton)");
    net::placeholder().await;
    Ok(())
}
