//! 手工验收用：对着本机跑着的 `poise serve` 走一遍网页流程。
//!
//! 新建 → 预览 → 确认启动 → 暂停 → 恢复 → 停止，全部走 HTTP 接口，用的就是网页
//! 调的那几个路径。跑之前先起服务：
//!
//! ```text
//! cargo run -p poise-cli -- --instance testnet serve --step-interval-secs 3
//! cargo run -p poise-cli --example web_flow -- http://127.0.0.1:8010
//! ```

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// 这次验收用的参数：买价全部落在标记价下方，不会一挂上去就成交。
const CONFIG: &str = r#"{
  "instrument": "BTC",
  "long": {
    "price_low": "78000",
    "price_high": "79500",
    "max_position": "0.004",
    "core_ratio": "0.5",
    "batch_count": 2,
    "grid_spacing": "500",
    "grid_spread": "750",
    "throttle_interval": null
  }
}"#;

fn main() {
    let base = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:8010".to_string());
    let host = base.trim_start_matches("http://").to_string();

    // 0. 网页本身：刷新任意一个前端路由都要能拿到 index.html。
    let (status, body) = request(&host, "GET", "/strategies/whatever", None);
    println!(
        "网页        {status} index.html {}",
        body.contains("<div id=\"root\">")
    );

    // 1. 新建草稿。
    let (status, body) = request(&host, "POST", "/api/drafts", Some(CONFIG));
    let draft_id = field(&body, "draft_id").expect("no draft id");
    println!("新建草稿    {status} {draft_id}");

    // 2. 预览。
    let (status, body) = request(
        &host,
        "POST",
        &format!("/api/drafts/{draft_id}/preview"),
        Some("{}"),
    );
    println!(
        "预览        {status} 校验 {} 格子 {} 立即成交 {} 标记价 {}",
        field(&body, "valid").unwrap_or_default(),
        count(&body, "\"entry_price\""),
        // 只数立即成交那一段：预览里的「第一步挂出的订单」也带限价。
        section(&body, "\"immediate_fills\"", "\"planned_orders\"")
            .map_or(0, |part| count(part, "\"limit_price\"")),
        field(&body, "market_price").unwrap_or_default(),
    );
    let token = field(&body, "token").expect("no confirm token");

    // 3. 确认启动。
    let (status, body) = request(
        &host,
        "POST",
        &format!("/api/drafts/{draft_id}/start"),
        Some(&format!("{{\"token\":\"{token}\"}}")),
    );
    println!(
        "确认启动    {status} {}",
        field(&body, "strategy_id").unwrap_or(body.clone())
    );

    std::thread::sleep(Duration::from_secs(5));
    show(&host, &draft_id, "运行中");

    // 4. 暂停。
    let (status, body) = request(
        &host,
        "POST",
        &format!("/api/strategies/{draft_id}/pause"),
        Some("{}"),
    );
    println!("暂停        {status} 撤销 {} 张", count(&body, "0x"));
    show(&host, &draft_id, "暂停后");

    // 5. 恢复。
    let (status, _) = request(
        &host,
        "POST",
        &format!("/api/strategies/{draft_id}/resume"),
        Some("{}"),
    );
    println!("恢复        {status}");
    std::thread::sleep(Duration::from_secs(5));
    show(&host, &draft_id, "恢复后");

    // 6. 停止，然后等后台把撤单收尾走完、归档。
    let (status, body) = request(
        &host,
        "POST",
        &format!("/api/strategies/{draft_id}/stop"),
        Some("{}"),
    );
    println!("停止        {status} 撤销 {} 张", count(&body, "0x"));
    show(&host, &draft_id, "停止后");

    for _ in 0..20 {
        std::thread::sleep(Duration::from_secs(1));
        let (_, body) = request(&host, "GET", &format!("/api/strategies/{draft_id}"), None);
        if field(&body, "status").as_deref() == Some("stopped") {
            println!("归档        {}", field(&body, "status").unwrap_or_default());
            return;
        }
    }
    println!("归档        等了一轮还没归档，看服务日志");
}

/// 读一次详情，打印状态、在场挂单数、快照时刻和说明。
fn show(host: &str, strategy_id: &str, label: &str) {
    let (status, body) = request(host, "GET", &format!("/api/strategies/{strategy_id}"), None);
    println!(
        "{label:<10}  {status} 状态 {} 在场挂单 {} 读数时刻 {} 说明 {}",
        field(&body, "status").unwrap_or_default(),
        count(&body, "\"client_order_id\""),
        field(&body, "taken_at").unwrap_or_default(),
        field(&body, "note").unwrap_or_else(|| "-".to_string()),
    );
}

/// 一个最小的 HTTP/1.1 请求，返回状态码和响应体。
fn request(host: &str, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
    let body = body.unwrap_or("");
    let mut stream = TcpStream::connect(host).expect("cannot connect to the console");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (head, body) = response
        .split_once("\r\n\r\n")
        .unwrap_or((response.as_str(), ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, body.to_string())
}

/// 从 JSON 里取出一个字符串或数字字段。
///
/// 验收脚本，不引 JSON 库：只找 `"名字":` 后面那一段。
fn field(body: &str, name: &str) -> Option<String> {
    let key = format!("\"{name}\":");
    let rest = &body[body.find(&key)? + key.len()..];
    let rest = rest.trim_start();
    if let Some(inner) = rest.strip_prefix('"') {
        return Some(inner[..inner.find('"')?].to_string());
    }
    if rest.starts_with("null") {
        return None;
    }
    let end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-')
        .unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// 数一个键出现了几次，用来数列表里的元素。
/// `body` 里从 `start` 到 `end` 之间的那一段。
fn section<'a>(body: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = body.find(start)?;
    let to = body[from..]
        .find(end)
        .map_or(body.len(), |offset| from + offset);
    Some(&body[from..to])
}

fn count(body: &str, key: &str) -> usize {
    body.matches(key).count()
}
