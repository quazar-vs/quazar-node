//! Custom error types for the trading bot.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum BotError {
    #[error("insufficient balance to open lot={lot}: available={available}")]
    InsufficientBalance { lot: f64, available: f64 },

    /// Soft-SL ladder finished (entry + configured max adds) — freeze TP until fill.
    #[error(
        "soft SL depth exceeded: steps={steps} max_adds={max_adds} next_lot={lot}"
    )]
    SoftSlDepthExceeded {
        steps: u32,
        max_adds: u32,
        lot: f64,
    },

    #[error("CoinEx API error code={code}: {message}")]
    Api { code: i64, message: String },

    #[error("websocket error: {0}")]
    WebSocket(String),

    #[error("invalid market precision data")]
    InvalidPrecision,

    #[error("bot stopped")]
    Stopped,
}
