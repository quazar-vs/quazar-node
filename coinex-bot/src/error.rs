//! Custom error types for the trading bot.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum BotError {
    #[error("insufficient balance to open lot={lot}: available={available}")]
    InsufficientBalance { lot: f64, available: f64 },

    #[error("lot {lot} exceeds configured max_lot {max_lot}")]
    LotExceedsMax { lot: f64, max_lot: f64 },

    #[error("CoinEx API error code={code}: {message}")]
    Api { code: i64, message: String },

    #[error("websocket error: {0}")]
    WebSocket(String),

    #[error("invalid market precision data")]
    InvalidPrecision,

    #[error("bot stopped")]
    Stopped,
}
