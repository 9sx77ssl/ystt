//! `ystt-api` server entry point.

use std::error::Error;

use tracing::info;
use ystt_api::{ApiConfig, AppState, router};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("ystt_api=info,tower_http=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();

    let config = ApiConfig::from_env()?;
    let bind = config.bind;
    let public_base = config.public_base.clone();
    let state = AppState::new(config)?;
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    info!(%bind, %public_base, "ystt API listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
