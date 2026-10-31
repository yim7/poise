//! 飞书机器人：把 runtime 的提醒（[`poise_runtime::Notice`]）推到群里。
//!
//! 地址来自启动文件的 `feishu_webhook`，它本身就是凭据（知道地址就能往群里发消息），所以
//! 只在这里用，不进日志和报错。发送在后台任务里排队做：提醒不能拖住下单、撤单和止损；
//! 送不出去只记日志。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use poise_runtime::{Notice, Notifier};
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// 同一个 `key`、同一段话在这段时间里只送一次：同一件事每一步都报一遍会刷屏。
const REPEAT_WINDOW: Duration = Duration::from_secs(10 * 60);

/// 两条之间至少隔这么久：飞书自定义机器人每秒最多 5 条。
const SEND_GAP: Duration = Duration::from_millis(300);

/// 往飞书发提醒的出口。
pub struct FeishuNotifier {
    queue: mpsc::UnboundedSender<Notice>,
    /// 排着队、还没处理完的条数：退出前等它们送出去（[`FeishuNotifier::flush`]）。
    pending: Arc<AtomicUsize>,
}

impl FeishuNotifier {
    /// 起一个后台任务发提醒。`label` 放在每条消息开头，说明是哪个实例。
    pub fn spawn(webhook: String, label: String) -> Result<Arc<Self>> {
        let client = client()?;
        let (queue, mut received) = mpsc::unbounded_channel::<Notice>();
        let pending = Arc::new(AtomicUsize::new(0));
        let left = pending.clone();
        tokio::spawn(async move {
            let mut sent: HashMap<String, (String, Instant)> = HashMap::new();
            while let Some(notice) = received.recv().await {
                let text = text(&label, &notice);
                if let Some((last, at)) = sent.get(&notice.key)
                    && *last == text
                    && at.elapsed() < REPEAT_WINDOW
                {
                    left.fetch_sub(1, Ordering::SeqCst);
                    continue;
                }
                sent.retain(|_, (_, at)| at.elapsed() < REPEAT_WINDOW);
                sent.insert(notice.key.clone(), (text.clone(), Instant::now()));
                if let Err(error) = send(&client, &webhook, &text).await {
                    tracing::warn!(error = %format!("{error:#}"), title = %notice.title, "cannot send the notice to Feishu");
                }
                left.fetch_sub(1, Ordering::SeqCst);
                tokio::time::sleep(SEND_GAP).await;
            }
        });
        Ok(Arc::new(Self { queue, pending }))
    }

    /// 等排着队的提醒送完，最多等 `limit`。
    pub async fn flush(&self, limit: Duration) {
        let started = Instant::now();
        while self.pending.load(Ordering::SeqCst) > 0 && started.elapsed() < limit {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

impl Notifier for FeishuNotifier {
    fn notify(&self, notice: Notice) {
        // 后台任务只会在进程退出时结束，那时丢掉提醒也无妨。
        self.pending.fetch_add(1, Ordering::SeqCst);
        if self.queue.send(notice).is_err() {
            self.pending.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// 立即发一条，等飞书的回复：`notify-test` 用它确认地址真的能用。
pub async fn send_now(webhook: &str, label: &str, notice: &Notice) -> Result<()> {
    send(&client()?, webhook, &text(label, notice)).await
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("cannot build the HTTP client for Feishu")
}

fn text(label: &str, notice: &Notice) -> String {
    if notice.body.is_empty() {
        format!("[{label}] {}", notice.title)
    } else {
        format!("[{label}] {}\n{}", notice.title, notice.body)
    }
}

/// 发一条文本消息。断网之类的错误重试一次；飞书明确拒绝（关键词、签名校验没过）不重试。
async fn send(client: &reqwest::Client, webhook: &str, text: &str) -> Result<()> {
    let body = json!({"msg_type": "text", "content": {"text": text}});
    let mut attempt = 0;
    let response = loop {
        attempt += 1;
        match client.post(webhook).json(&body).send().await {
            Ok(response) => break response,
            // 报错里不带地址：`reqwest` 的错误会把 URL 原样写出来。
            Err(error) if attempt < 2 => {
                tracing::debug!(error = %error.without_url(), "retrying the Feishu notice");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Err(error) => bail!("cannot reach Feishu: {}", error.without_url()),
        }
    };
    let status = response.status();
    let reply: Value = response.json().await.map_err(|error| {
        anyhow::anyhow!(
            "Feishu replied {status} with an unreadable body: {}",
            error.without_url()
        )
    })?;
    let code = reply.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if !status.is_success() || code != 0 {
        let message = reply.get("msg").and_then(Value::as_str).unwrap_or("");
        bail!("Feishu refused the notice: HTTP {status}, code {code}, {message}");
    }
    Ok(())
}
