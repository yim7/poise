use std::fmt;

use reqwest::StatusCode;

use poise_engine::ports::ExecutionPortErrorKind;

#[derive(Debug, Clone)]
pub(crate) enum HyperliquidRestError {
    HttpStatus {
        path: String,
        status: StatusCode,
        body: String,
    },
    Exchange {
        response: String,
    },
    OrderRejected {
        message: String,
    },
}

impl HyperliquidRestError {
    pub(crate) fn http_status(path: impl Into<String>, status: StatusCode, body: String) -> Self {
        Self::HttpStatus {
            path: path.into(),
            status,
            body,
        }
    }

    pub(crate) fn exchange_response(response: serde_json::Value) -> Self {
        Self::Exchange {
            response: response.to_string(),
        }
    }

    pub(crate) fn order_rejected(message: impl Into<String>) -> Self {
        Self::OrderRejected {
            message: message.into(),
        }
    }

    pub(crate) fn execution_error_kind(&self) -> Option<ExecutionPortErrorKind> {
        if self.is_insufficient_margin() {
            return Some(ExecutionPortErrorKind::InsufficientMargin);
        }
        if self.is_invalid_price_increment() {
            return Some(ExecutionPortErrorKind::InvalidPriceIncrement);
        }
        if self.is_rate_limited() {
            return Some(ExecutionPortErrorKind::RateLimited);
        }
        None
    }

    fn is_insufficient_margin(&self) -> bool {
        let message = normalized(self.message());
        message.contains("insufficient")
            && (message.contains("margin")
                || message.contains("collateral")
                || message.contains("balance"))
    }

    fn is_invalid_price_increment(&self) -> bool {
        let message = normalized(self.message());
        message.contains("tick size") || message.contains("divisible by tick")
    }

    fn is_rate_limited(&self) -> bool {
        matches!(
            self,
            Self::HttpStatus {
                status: StatusCode::TOO_MANY_REQUESTS,
                ..
            }
        )
    }

    fn message(&self) -> &str {
        match self {
            Self::HttpStatus { body, .. } => body,
            Self::Exchange { response } => response,
            Self::OrderRejected { message } => message,
        }
    }
}

impl fmt::Display for HyperliquidRestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HttpStatus { path, status, body } => {
                write!(f, "request POST {path} failed with status {status}: {body}")
            }
            Self::Exchange { response } => write!(f, "Hyperliquid exchange error: {response}"),
            Self::OrderRejected { message } => {
                write!(f, "Hyperliquid order rejected: {message}")
            }
        }
    }
}

impl std::error::Error for HyperliquidRestError {}

fn normalized(value: &str) -> String {
    value.to_ascii_lowercase()
}
