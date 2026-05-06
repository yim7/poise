use std::fmt;

use reqwest::{Method, StatusCode};

use poise_engine::ports::ExecutionPortErrorKind;

#[derive(Debug, Clone)]
pub(crate) struct BybitRestError {
    method: Method,
    path: String,
    status: Option<StatusCode>,
    body: Option<String>,
    ret_code: Option<i64>,
    ret_msg: Option<String>,
}

impl BybitRestError {
    pub(crate) fn http_status(
        method: Method,
        path: impl Into<String>,
        status: StatusCode,
        body: String,
    ) -> Self {
        Self {
            method,
            path: path.into(),
            status: Some(status),
            body: Some(body),
            ret_code: None,
            ret_msg: None,
        }
    }

    pub(crate) fn ret_code(
        method: Method,
        path: impl Into<String>,
        ret_code: i64,
        ret_msg: Option<String>,
    ) -> Self {
        Self {
            method,
            path: path.into(),
            status: None,
            body: None,
            ret_code: Some(ret_code),
            ret_msg,
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
        let message = normalized(self.ret_msg.as_deref().unwrap_or_default());
        matches!(self.ret_code, Some(110006 | 110007 | 110012 | 110014))
            || (message.contains("insufficient")
                && (message.contains("margin")
                    || message.contains("balance")
                    || message.contains("collateral")))
    }

    fn is_invalid_price_increment(&self) -> bool {
        let message = normalized(self.ret_msg.as_deref().unwrap_or_default());
        message.contains("tick")
            || message.contains("price precision")
            || message.contains("price scale")
    }

    fn is_rate_limited(&self) -> bool {
        self.status == Some(StatusCode::TOO_MANY_REQUESTS) || self.ret_code == Some(10006)
    }
}

impl fmt::Display for BybitRestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.status, self.ret_code) {
            (Some(status), _) => write!(
                f,
                "request {} {} failed with status {}: {}",
                self.method,
                self.path,
                status,
                self.body.as_deref().unwrap_or_default()
            ),
            (_, Some(ret_code)) => write!(
                f,
                "request {} {} failed with retCode {}: {}",
                self.method,
                self.path,
                ret_code,
                self.ret_msg.as_deref().unwrap_or_default()
            ),
            _ => write!(f, "request {} {} failed", self.method, self.path),
        }
    }
}

impl std::error::Error for BybitRestError {}

fn normalized(value: &str) -> String {
    value.to_ascii_lowercase()
}
