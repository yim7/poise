//! 离线测试用的假出口。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use poise_runtime::{Address, ExchangeError, Network, SecretKey};
use serde_json::Value;

use crate::client::{Clock, Hyperliquid};
use crate::transport::Transport;

/// 测试私钥 0x…01，以及它推出的地址。
pub(crate) const PRIVATE_KEY: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000001";
pub(crate) const API_WALLET: &str = "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf";
pub(crate) const ACCOUNT: &str = "0x2222222222222222222222222222222222222222";

/// 一次请求。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Request {
    pub path: String,
    pub body: Value,
}

/// 按脚本回复的出口，同时记下收到的请求。
pub(crate) struct FakeTransport {
    responses: Mutex<std::collections::VecDeque<Result<Value, ExchangeError>>>,
    requests: Mutex<Vec<Request>>,
}

impl FakeTransport {
    /// 依次回复这些响应。
    pub(crate) fn new(responses: Vec<Value>) -> Arc<Self> {
        Self::scripted(responses.into_iter().map(Ok).collect())
    }

    /// 按脚本先成功、后失败（或任意成败组合）。
    pub(crate) fn scripted(responses: Vec<Result<Value, ExchangeError>>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    pub(crate) fn request(&self, index: usize) -> Request {
        self.requests().get(index).cloned().unwrap_or_else(|| {
            panic!(
                "the fake transport only received {} requests",
                self.requests().len()
            )
        })
    }

    /// `/info` 请求的 `type` 序列，用来看请求顺序和次数。
    pub(crate) fn info_types(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .filter(|request| request.path == crate::transport::INFO_PATH)
            .map(|request| {
                request.body["type"]
                    .as_str()
                    .unwrap_or("<missing>")
                    .to_string()
            })
            .collect()
    }
}

#[async_trait]
impl Transport for FakeTransport {
    async fn post(&self, path: &str, body: &Value) -> Result<Value, ExchangeError> {
        self.requests.lock().unwrap().push(Request {
            path: path.to_string(),
            body: body.clone(),
        });
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("the fake transport ran out of responses for POST {path}"))
    }
}

/// 固定时刻的时钟。
pub(crate) fn fixed_clock(now_ms: u64) -> Clock {
    Arc::new(move || now_ms)
}

/// 可以往前拨的时钟。
pub(crate) fn advanceable_clock(start_ms: u64) -> (Arc<AtomicU64>, Clock) {
    let now = Arc::new(AtomicU64::new(start_ms));
    let clock_handle = Arc::clone(&now);
    let clock: Clock = Arc::new(move || clock_handle.load(Ordering::SeqCst));
    (now, clock)
}

pub(crate) fn client(transport: Arc<FakeTransport>, clock: Clock) -> Hyperliquid {
    Hyperliquid::with_transport(
        transport,
        Network::Testnet,
        Address::parse(ACCOUNT).unwrap(),
        SecretKey::parse(PRIVATE_KEY).unwrap(),
        clock,
    )
}

/// 时钟停在 `now_ms` 的客户端。
pub(crate) fn fake_client(now_ms: u64) -> Hyperliquid {
    client(FakeTransport::new(Vec::new()), fixed_clock(now_ms))
}
