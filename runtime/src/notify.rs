//! 对外提醒：需要用户动手、或者用户不看盘时也该知道的事（失效、自动暂停、兜底单触发、
//! 保证金率越线、驱动连续失败、反复撤挂）。
//!
//! runtime 只说「发生了什么」，怎么送出去由调用方注入的 [`Notifier`] 决定（命令行接的是
//! 飞书机器人）。提醒是尽力而为：送不出去只记日志，不能挡住下单、撤单和止损。
//!
//! 提醒里不写仓位、数量和金额：控制台默认遮住这些会暴露账户规模的数字，推到群里更不该带。
//! 价格照常写。

use std::sync::Arc;

/// 一条提醒。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// 去重用：同一个 `key` 的提醒短时间内只送一次（见实现方）。
    pub key: String,
    /// 一句话说清楚发生了什么，例如「BTC 已暂停」。
    pub title: String,
    /// 补充说明，可以为空。
    pub body: String,
}

impl Notice {
    pub fn new(key: impl Into<String>, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            title: title.into(),
            body: body.into(),
        }
    }
}

/// 提醒的出口。实现必须立即返回：真正的发送放到后台去做。
pub trait Notifier: Send + Sync {
    fn notify(&self, notice: Notice);
}

/// 不送任何提醒：没配置提醒渠道时、以及测试里的缺省值。
pub struct NoNotifier;

impl Notifier for NoNotifier {
    fn notify(&self, _notice: Notice) {}
}

/// 测试用：把收到的提醒按顺序记下来。
#[derive(Default)]
pub struct RecordingNotifier {
    notices: std::sync::Mutex<Vec<Notice>>,
}

impl RecordingNotifier {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn notices(&self) -> Vec<Notice> {
        self.notices.lock().expect("notices lock poisoned").clone()
    }

    pub fn titles(&self) -> Vec<String> {
        self.notices()
            .into_iter()
            .map(|notice| notice.title)
            .collect()
    }
}

impl Notifier for RecordingNotifier {
    fn notify(&self, notice: Notice) {
        self.notices
            .lock()
            .expect("notices lock poisoned")
            .push(notice);
    }
}
