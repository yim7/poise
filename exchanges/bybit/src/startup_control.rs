use std::sync::Arc;

use anyhow::Result;

use crate::{Config, Deployment, rest::BybitRestClient};

pub struct SymbolLeverageControl {
    rest: SymbolLeverageRest,
}

enum SymbolLeverageRest {
    Live {
        deployment: Deployment,
        api_key: String,
        api_secret: String,
    },
    #[allow(dead_code)]
    Injected(Arc<BybitRestClient>),
}

impl SymbolLeverageControl {
    pub fn new(config: &Config) -> Result<Self> {
        let (api_key, api_secret) = config.credentials()?;
        Ok(Self {
            rest: SymbolLeverageRest::Live {
                deployment: config.deployment.clone(),
                api_key,
                api_secret,
            },
        })
    }

    #[cfg(test)]
    fn from_rest_client(rest: Arc<BybitRestClient>) -> Self {
        Self {
            rest: SymbolLeverageRest::Injected(rest),
        }
    }

    pub async fn set_leverage(&self, symbol: &str, leverage: u32) -> Result<()> {
        match &self.rest {
            SymbolLeverageRest::Live {
                deployment,
                api_key,
                api_secret,
            } => {
                BybitRestClient::new(deployment.clone(), api_key.clone(), api_secret.clone())
                    .set_leverage(symbol, leverage)
                    .await
            }
            SymbolLeverageRest::Injected(rest) => rest.set_leverage(symbol, leverage).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::Arc;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::SymbolLeverageControl;
    use crate::rest::BybitRestClient;

    #[tokio::test]
    async fn startup_control_forwards_symbol_and_leverage_to_rest_client() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{"retCode":0,"retMsg":"OK","result":{},"retExtInfo":{},"time":1672281607343}"#,
        )])
        .await;
        let control = SymbolLeverageControl::from_rest_client(Arc::new(
            BybitRestClient::with_http_client_and_timestamp_provider(
                server.base_url(),
                "api-key",
                "secret-key",
                Arc::new(|| 1_700_000_000_000),
                reqwest::Client::new(),
            ),
        ));

        control.set_leverage("BTCUSDT", 10).await.unwrap();

        let request = &server.requests()[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v5/position/set-leverage");
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

    struct MockHttpServer {
        base_url: String,
        requests: Arc<std::sync::Mutex<Vec<RecordedRequest>>>,
    }

    impl MockHttpServer {
        async fn spawn(responses: Vec<MockResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
            let queued_responses = Arc::new(std::sync::Mutex::new(VecDeque::from(responses)));
            let stored_requests = Arc::clone(&requests);

            tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        break;
                    };
                    let mut buffer = Vec::new();
                    let mut chunk = [0_u8; 1024];

                    loop {
                        let read = socket.read(&mut chunk).await.unwrap();
                        if read == 0 {
                            break;
                        }
                        buffer.extend_from_slice(&chunk[..read]);

                        let request_text = String::from_utf8_lossy(&buffer);
                        let Some((head, body)) = request_text.split_once("\r\n\r\n") else {
                            continue;
                        };
                        let content_length = head
                            .lines()
                            .find_map(|line| line.split_once(':'))
                            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if body.len() >= content_length {
                            break;
                        }
                    }

                    if buffer.is_empty() {
                        break;
                    }
                    let request = parse_request(&String::from_utf8_lossy(&buffer));
                    stored_requests.lock().unwrap().push(request);

                    let response = queued_responses.lock().unwrap().pop_front().unwrap();
                    let status_text = if response.status == 200 { "OK" } else { "ERR" };
                    let raw = format!(
                        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
                        response.status,
                        status_text,
                        response.body.len(),
                        response.body
                    );
                    socket.write_all(raw.as_bytes()).await.unwrap();
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

        fn requests(&self) -> Vec<RecordedRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct RecordedRequest {
        method: String,
        path: String,
        headers: HashMap<String, String>,
        body: String,
    }

    fn parse_request(raw: &str) -> RecordedRequest {
        let (head, body) = raw
            .split_once("\r\n\r\n")
            .map(|(head, body)| (head, body.to_string()))
            .unwrap_or((raw, String::new()));
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap();
        let mut request_parts = request_line.split_whitespace();
        let method = request_parts.next().unwrap().to_string();
        let path = request_parts.next().unwrap().to_string();
        let mut headers = HashMap::new();
        for line in lines {
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        RecordedRequest {
            method,
            path,
            headers,
            body,
        }
    }
}
