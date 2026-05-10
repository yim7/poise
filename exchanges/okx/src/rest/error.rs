use std::fmt;

use reqwest::{Method, StatusCode};

use poise_engine::ports::ExecutionPortErrorKind;

#[derive(Debug, Clone)]
pub(crate) enum OkxRestError {
    HttpStatus {
        method: Method,
        path: String,
        status: StatusCode,
        body: String,
    },
    Code {
        method: Method,
        path: String,
        code: String,
        message: String,
    },
    Acknowledgement {
        order_id: String,
        code: String,
        message: String,
    },
}

impl OkxRestError {
    pub(crate) fn http_status(
        method: Method,
        path: impl Into<String>,
        status: StatusCode,
        body: String,
    ) -> Self {
        Self::HttpStatus {
            method,
            path: path.into(),
            status,
            body,
        }
    }

    pub(crate) fn business_code(
        method: Method,
        path: impl Into<String>,
        code: String,
        message: String,
    ) -> Self {
        Self::Code {
            method,
            path: path.into(),
            code,
            message,
        }
    }

    pub(crate) fn acknowledgement(
        order_id: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::Acknowledgement {
            order_id: order_id.into(),
            code: code.into(),
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
        if self.is_cancel_outcome_unknown() {
            return Some(ExecutionPortErrorKind::CancelOutcomeUnknown);
        }
        None
    }

    fn is_insufficient_margin(&self) -> bool {
        let message = normalized(self.message());
        self.error_code() == Some("51008")
            || (message.contains("insufficient")
                && (message.contains("margin")
                    || message.contains("balance")
                    || message.contains("collateral")))
    }

    fn is_invalid_price_increment(&self) -> bool {
        let message = normalized(self.message());
        message.contains("tick")
            || message.contains("price precision")
            || message.contains("price increment")
    }

    fn is_rate_limited(&self) -> bool {
        self.status() == Some(StatusCode::TOO_MANY_REQUESTS) || self.error_code() == Some("50011")
    }

    fn is_cancel_outcome_unknown(&self) -> bool {
        self.error_code() == Some("51400")
            && normalized(self.message()).contains("order cancellation failed")
    }

    fn status(&self) -> Option<StatusCode> {
        match self {
            Self::HttpStatus { status, .. } => Some(*status),
            Self::Code { .. } | Self::Acknowledgement { .. } => None,
        }
    }

    fn error_code(&self) -> Option<&str> {
        match self {
            Self::Code { code, .. } | Self::Acknowledgement { code, .. } => Some(code.as_str()),
            Self::HttpStatus { .. } => None,
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::HttpStatus { body, .. } => body,
            Self::Code { message, .. } | Self::Acknowledgement { message, .. } => message,
        }
    }
}

impl fmt::Display for OkxRestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HttpStatus {
                method,
                path,
                status,
                body,
            } => write!(
                f,
                "request {method} {path} failed with status {status}: {body}"
            ),
            Self::Code {
                method,
                path,
                code,
                message,
            } => write!(
                f,
                "request {method} {path} failed with OKX code {code}: {message}"
            ),
            Self::Acknowledgement {
                order_id,
                code,
                message,
            } => write!(
                f,
                "OKX acknowledgement for order `{order_id}` failed with sCode {code}: {message}"
            ),
        }
    }
}

impl std::error::Error for OkxRestError {}

fn normalized(value: &str) -> String {
    value.to_ascii_lowercase()
}
