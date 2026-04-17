use std::sync::Arc;

use anyhow::Result;

use crate::{Config, rest::BinanceRestClient};

pub struct SymbolLeverageControl {
    rest: Arc<BinanceRestClient>,
}

impl SymbolLeverageControl {
    pub fn new(config: &Config) -> Result<Self> {
        let endpoints = config.endpoints();
        let (api_key, api_secret) = config.credentials()?;
        Ok(Self {
            rest: Arc::new(BinanceRestClient::new(
                endpoints.rest_base_url(),
                api_key,
                api_secret,
            )),
        })
    }

    #[cfg(test)]
    fn from_rest_client(rest: Arc<BinanceRestClient>) -> Self {
        Self { rest }
    }

    pub async fn set_leverage(&self, symbol: &str, leverage: u32) -> Result<()> {
        self.rest.set_leverage(symbol, leverage).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::Arc;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::Mutex,
    };

    use super::SymbolLeverageControl;
    use crate::rest::BinanceRestClient;

    #[tokio::test]
    async fn startup_control_forwards_symbol_and_leverage_to_rest_client() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{"leverage":10,"maxNotionalValue":"1000000","symbol":"BTCUSDT"}"#,
        )])
        .await;
        let control = SymbolLeverageControl::from_rest_client(Arc::new(BinanceRestClient::new(
            server.base_url(),
            "api-key",
            "secret-key",
        )));

        control.set_leverage("BTCUSDT", 10).await.unwrap();

        let requests = server.requests().await;
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]
                .path
                .starts_with("/fapi/v1/leverage?symbol=BTCUSDT&leverage=10&timestamp=")
        );
    }

    #[derive(Debug, Clone)]
    struct MockResponse {
        status: u16,
        body: String,
    }

    impl MockResponse {
        fn json(status: u16, body: &str) -> Self {
            Self {
                status,
                body: body.to_string(),
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct RecordedRequest {
        method: String,
        path: String,
        headers: HashMap<String, String>,
    }

    struct MockHttpServer {
        base_url: String,
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
    }

    impl MockHttpServer {
        async fn spawn(responses: Vec<MockResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let queued_responses = Arc::new(Mutex::new(VecDeque::from(responses)));
            let stored_requests = Arc::clone(&requests);

            tokio::spawn(async move {
                loop {
                    let response = {
                        let mut queue = queued_responses.lock().await;
                        queue.pop_front()
                    };

                    let Some(response) = response else {
                        break;
                    };

                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut buffer = Vec::new();
                    let mut chunk = [0_u8; 1024];

                    loop {
                        let read = stream.read(&mut chunk).await.unwrap();
                        if read == 0 {
                            break;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }

                    let request_text = String::from_utf8(buffer).unwrap();
                    let mut lines = request_text.split("\r\n");
                    let request_line = lines.next().unwrap();
                    let mut request_line_parts = request_line.split_whitespace();
                    let method = request_line_parts.next().unwrap().to_string();
                    let path = request_line_parts.next().unwrap().to_string();
                    let mut headers = HashMap::new();

                    for line in lines.by_ref() {
                        if line.is_empty() {
                            break;
                        }
                        if let Some((name, value)) = line.split_once(':') {
                            headers
                                .insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
                        }
                    }

                    stored_requests.lock().await.push(RecordedRequest {
                        method,
                        path,
                        headers,
                    });

                    let reply = format!(
                        "HTTP/1.1 {} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        response.status,
                        response.body.len(),
                        response.body
                    );

                    stream.write_all(reply.as_bytes()).await.unwrap();
                    stream.shutdown().await.unwrap();
                }
            });

            Self {
                base_url: format!("http://{}", address),
                requests,
            }
        }

        fn base_url(&self) -> String {
            self.base_url.clone()
        }

        async fn requests(&self) -> Vec<RecordedRequest> {
            self.requests.lock().await.clone()
        }
    }
}
