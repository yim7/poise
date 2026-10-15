//! HTTP 出口。
//!
//! 只有两个端点：`/info` 读、`/exchange` 写。适配层其余部分只依赖 [`Transport`]，
//! 所以离线测试可以完全绕开网络。
//!
//! 报错里只有路径、HTTP 状态和「解析失败」的位置，没有响应正文：私密端点的响应
//! 里是账户数据，不该出现在日志里。

use std::time::Duration;

use async_trait::async_trait;
use poise_runtime::ExchangeError;
use serde_json::Value;

pub(crate) const INFO_PATH: &str = "/info";
pub(crate) const EXCHANGE_PATH: &str = "/exchange";

/// 单次请求的最长等待时间。超时不代表没送到，所以调用方按结果未知处理。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[async_trait]
pub(crate) trait Transport: Send + Sync {
    async fn post(&self, path: &str, body: &Value) -> Result<Value, ExchangeError>;
}

pub(crate) fn build_http_client() -> Result<reqwest::Client, ExchangeError> {
    reqwest::Client::builder()
        // 不跟着重定向走：下单的重定向会让结果变得不可判定。
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| ExchangeError::Unavailable {
            reason: format!("cannot build the HTTP client: {error}"),
        })
}

pub(crate) struct HttpTransport {
    http: reqwest::Client,
    base_url: String,
}

impl HttpTransport {
    pub(crate) fn new(base_url: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn post(&self, path: &str, body: &Value) -> Result<Value, ExchangeError> {
        let url = format!("{}{path}", self.base_url);
        let response = self
            .http
            .post(&url)
            .json(body)
            .send()
            .await
            .map_err(|error| failure(path, format!("POST {path} failed: {error}")))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| failure(path, format!("cannot read the {path} response: {error}")))?;
        if !status.is_success() {
            return Err(failure(path, format!("POST {path} returned {status}")));
        }
        serde_json::from_str(&text).map_err(|error| {
            failure(
                path,
                format!(
                    "cannot parse the {path} response: {} error at line {} column {}",
                    category(&error),
                    error.line(),
                    error.column()
                ),
            )
        })
    }
}

/// `/info` 读失败什么都没发生；`/exchange` 写失败有可能已经送到，必须当成结果未知。
fn failure(path: &str, reason: String) -> ExchangeError {
    if path == EXCHANGE_PATH {
        ExchangeError::Unknown { reason }
    } else {
        ExchangeError::Unavailable { reason }
    }
}

/// JSON 解析失败的大类。只说类别和位置，不回显正文。
pub(crate) fn category(error: &serde_json::Error) -> &'static str {
    match error.classify() {
        serde_json::error::Category::Io => "I/O",
        serde_json::error::Category::Syntax => "syntax",
        serde_json::error::Category::Data => "data",
        serde_json::error::Category::Eof => "end-of-input",
    }
}

#[cfg(test)]
mod tests {
    use poise_runtime::ExchangeError;

    use super::{EXCHANGE_PATH, INFO_PATH, failure};

    #[test]
    fn a_failed_write_is_an_unknown_result_and_a_failed_read_is_unavailable() {
        assert!(matches!(
            failure(EXCHANGE_PATH, "timeout".to_string()),
            ExchangeError::Unknown { .. }
        ));
        assert!(matches!(
            failure(INFO_PATH, "timeout".to_string()),
            ExchangeError::Unavailable { .. }
        ));
    }
}
