use crate::domain::state::PaymentState;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid amount: {0:?}")]
    InvalidAmount(String),

    #[error("invalid address: {0:?}")]
    InvalidAddress(String),

    #[error("proposal is not valid JSON for the expected schema: {0}")]
    Json(#[from] serde_json::Error),

    #[error("malformed proposal: {0}")]
    MalformedProposal(String),

    #[error("invalid policy configuration: {0}")]
    InvalidPolicy(String),

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("payment {0} not found")]
    PaymentNotFound(String),

    #[error("stored data is inconsistent: {0}")]
    Corrupt(String),

    #[error("transaction does not match the approved payment: {0}")]
    MessageMismatch(String),

    #[error("signer error: {0}")]
    Signer(String),

    #[error("chain error: {0}")]
    Chain(String),

    #[error("could not understand the request: {0}")]
    Unparseable(String),

    #[error("language model error: {0}")]
    Llm(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("illegal payment state transition {from:?} -> {to:?}")]
    InvalidTransition {
        from: PaymentState,
        to: PaymentState,
    },
}

pub type Result<T> = std::result::Result<T, Error>;
