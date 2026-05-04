use std::sync::Arc;

use anyhow::Result;

use crate::{Config, rest::client::OkxRestClient};

pub struct SymbolLeverageControl {
    rest: Arc<OkxRestClient>,
}

impl SymbolLeverageControl {
    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            rest: Arc::new(OkxRestClient::new(config)?),
        })
    }

    #[cfg(test)]
    fn from_rest_client(rest: Arc<OkxRestClient>) -> Self {
        Self { rest }
    }

    pub async fn set_leverage(&self, symbol: &str, leverage: u32) -> Result<()> {
        self.rest.set_leverage(symbol, leverage).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};

    use chrono::{DateTime, Utc};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;
    use crate::rest::client::OkxRestClient;
    use crate::{Config, Deployment};

    #[tokio::test]
    async fn symbol_leverage_control_routes_rest_client() {
        let server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{"code":"0","msg":"","data":[{"instId":"BTC-USDT-SWAP","lever":"10","mgnMode":"cross"}]}"#,
        )])
        .await;
        let rest = Arc::new(OkxRestClient::with_http_client_and_timestamp_provider(
            server.base_url(),
            Config {
                deployment: Deployment::Demo,
                api_key: Some("api-key".to_string()),
                api_secret: Some("secret-key".to_string()),
                passphrase: Some("passphrase".to_string()),
            }
            .credentials()
            .unwrap(),
            true,
            Arc::new(fixed_datetime),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        ));
        let control = SymbolLeverageControl::from_rest_client(rest);

        control.set_leverage("BTC-USDT-SWAP", 10).await.unwrap();

        let request = &server.requests()[0];
        assert_eq!(request.path, "/api/v5/account/set-leverage");
        assert!(request.body.contains(r#""instId":"BTC-USDT-SWAP""#));
        assert!(request.body.contains(r#""lever":"10""#));
        assert!(request.body.contains(r#""mgnMode":"cross""#));
    }

    fn fixed_datetime() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2020-12-08T09:08:57.715Z")
            .unwrap()
            .with_timezone(&Utc)
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
                    let Ok((mut socket, _)) = listener.accept().await else {
                        break;
                    };
                    let mut buffer = Vec::new();
                    loop {
                        let mut chunk = [0_u8; 4096];
                        let read = socket.read(&mut chunk).await.unwrap();
                        if read == 0 {
                            break;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        if request_complete(&buffer) {
                            break;
                        }
                    }
                    if buffer.is_empty() {
                        break;
                    }
                    stored_requests
                        .lock()
                        .unwrap()
                        .push(parse_request(&String::from_utf8_lossy(&buffer)));

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
        path: String,
        headers: HashMap<String, String>,
        body: String,
    }

    fn request_complete(buffer: &[u8]) -> bool {
        let request_text = String::from_utf8_lossy(buffer);
        let Some((head, body)) = request_text.split_once("\r\n\r\n") else {
            return false;
        };
        let content_length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        body.len() >= content_length
    }

    fn parse_request(raw: &str) -> RecordedRequest {
        let (head, body) = raw
            .split_once("\r\n\r\n")
            .map(|(head, body)| (head, body.to_string()))
            .unwrap_or((raw, String::new()));
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap();
        let path = request_line.split_whitespace().nth(1).unwrap().to_string();
        let mut headers = HashMap::new();
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        RecordedRequest {
            path,
            headers,
            body,
        }
    }
}
