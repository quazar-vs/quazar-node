//! CoinEx one-sided martingale bot entrypoint.

mod bot;
mod config;
mod error;
mod exchange;
mod scanner;
mod strategy;

use crate::bot::Bot;
use crate::config::BotConfig;
use anyhow::Result;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    info!("starting CoinEx one-sided martingale bot");

    let config = BotConfig::from_env()?;
    info!(
        market = %config.market,
        mode = config.market_kind.label(),
        side = config.trade_side.as_str(),
        lot_coeff = config.lot_coeff,
        lot_step_coeff = config.lot_step_coeff,
        max_soft_sl_adds = config.max_soft_sl_adds,
        tp_sl_percent = config.tp_sl_percent,
        auto_market = config.auto_market,
        leverage = config.leverage,
        "configuration loaded (working min lot = exchange_min × LOT_COEFF after meta)"
    );

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut sigterm = tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::terminate(),
            )
            .expect("failed to install SIGTERM handler");
            tokio::select! {
                _ = ctrl_c => info!("received Ctrl+C"),
                _ = sigterm.recv() => info!("received SIGTERM"),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = ctrl_c.await;
            info!("received Ctrl+C");
        }
        let _ = shutdown_tx.send(true);
    });

    let bot = Bot::new(config).await?;
    if let Err(e) = bot.run(shutdown_rx).await {
        error!(error = %e, "bot terminated with error");
        return Err(e);
    }

    info!("bot stopped gracefully");
    Ok(())
}
