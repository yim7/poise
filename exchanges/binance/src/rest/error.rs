use std::fmt;

use reqwest::{Method, StatusCode};

use poise_engine::ports::ExecutionPortErrorKind;

use super::models::BinanceErrorResponse;

#[derive(Debug, Clone)]
pub(crate) struct BinanceRestError {
    method: Method,
    path: String,
    status: StatusCode,
    body: String,
    code: Option<i64>,
}

impl BinanceRestError {
    pub(crate) fn new(
        method: Method,
        path: impl Into<String>,
        status: StatusCode,
        body: String,
    ) -> Self {
        let code = serde_json::from_str::<BinanceErrorResponse>(&body)
            .ok()
            .map(|error| error.code);

        Self {
            method,
            path: path.into(),
            status,
            body,
            code,
        }
    }

    #[cfg(test)]
    pub(crate) fn code(&self) -> Option<i64> {
        self.code
    }

    #[cfg(test)]
    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }

    #[cfg(test)]
    pub(crate) fn body(&self) -> &str {
        &self.body
    }

    pub(crate) fn is_cancel_outcome_unknown(&self) -> bool {
        self.code == Some(-2011)
    }

    pub(crate) fn execution_error_kind(&self) -> Option<ExecutionPortErrorKind> {
        if self.is_cancel_outcome_unknown() {
            return Some(ExecutionPortErrorKind::CancelOutcomeUnknown);
        }
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
        self.code == Some(-2019) || self.body.contains("Margin is insufficient")
    }

    fn is_invalid_price_increment(&self) -> bool {
        self.code == Some(-4014) || self.body.to_ascii_lowercase().contains("tick size")
    }

    fn is_rate_limited(&self) -> bool {
        self.status == StatusCode::TOO_MANY_REQUESTS
            || self.status.as_u16() == 418
            || self.code == Some(-1003)
    }
}

impl fmt::Display for BinanceRestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "request {} {} failed with status {}: {}",
            self.method, self.path, self.status, self.body
        )
    }
}

impl std::error::Error for BinanceRestError {}

#[cfg(test)]
mod tests {
    use reqwest::{Method, StatusCode};

    use super::BinanceRestError;

    #[test]
    fn extracts_structured_code_from_response_body() {
        let error = BinanceRestError::new(
            Method::DELETE,
            "/fapi/v1/order",
            StatusCode::BAD_REQUEST,
            "{\"code\":-2011,\"msg\":\"Unknown order sent.\"}".to_string(),
        );

        assert_eq!(error.code(), Some(-2011));
    }

    #[test]
    fn leaves_code_empty_when_body_is_not_json() {
        let error = BinanceRestError::new(
            Method::DELETE,
            "/fapi/v1/order",
            StatusCode::BAD_REQUEST,
            "gateway timeout".to_string(),
        );

        assert_eq!(error.code(), None);
    }

    #[test]
    fn classifies_exchange_execution_error_kind() {
        let error = BinanceRestError::new(
            Method::POST,
            "/fapi/v1/order",
            StatusCode::BAD_REQUEST,
            "{\"code\":-2019,\"msg\":\"Margin is insufficient.\"}".to_string(),
        );

        assert_eq!(
            error.execution_error_kind(),
            Some(poise_engine::ports::ExecutionPortErrorKind::InsufficientMargin)
        );
    }
}
