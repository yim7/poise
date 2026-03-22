use crate::{
    protocol::{CommandType, GridLevelState, RiskLevel, StrategyStatus},
    selectors,
    state::CommandTimelineStage,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locale {
    EnUs,
    ZhCn,
}

impl Locale {
    pub fn from_env_value(raw: &str) -> Option<Self> {
        match raw {
            "en-US" => Some(Self::EnUs),
            "zh-CN" => Some(Self::ZhCn),
            _ => None,
        }
    }

    pub fn toggle(self) -> Self {
        match self {
            Self::EnUs => Self::ZhCn,
            Self::ZhCn => Self::EnUs,
        }
    }
}

pub fn copy(locale: Locale) -> UiCopy {
    UiCopy { locale }
}

#[derive(Debug, Clone, Copy)]
pub struct UiCopy {
    locale: Locale,
}

impl UiCopy {
    pub fn tabs(self) -> [&'static str; 5] {
        match self.locale {
            Locale::EnUs => ["Dashboard", "Grid", "Market", "Events", "Help"],
            Locale::ZhCn => ["概览", "网格", "行情", "事件", "帮助"],
        }
    }

    pub fn status(self) -> StatusCopy {
        StatusCopy {
            locale: self.locale,
        }
    }

    pub fn bootstrap(self) -> BootstrapCopy {
        BootstrapCopy {
            locale: self.locale,
        }
    }

    pub fn dashboard(self) -> DashboardCopy {
        DashboardCopy {
            locale: self.locale,
        }
    }

    pub fn grid(self) -> GridCopy {
        GridCopy {
            locale: self.locale,
        }
    }

    pub fn market(self) -> MarketCopy {
        MarketCopy {
            locale: self.locale,
        }
    }

    pub fn instances(self) -> InstancesCopy {
        InstancesCopy {
            locale: self.locale,
        }
    }

    pub fn events(self) -> EventsCopy {
        EventsCopy {
            locale: self.locale,
        }
    }

    pub fn help(self) -> HelpCopy {
        HelpCopy {
            locale: self.locale,
        }
    }

    pub fn footer(self) -> FooterCopy {
        FooterCopy {
            locale: self.locale,
        }
    }

    pub fn toast(self) -> ToastCopy {
        ToastCopy {
            locale: self.locale,
        }
    }

    pub fn store(self) -> StoreCopy {
        StoreCopy {
            locale: self.locale,
        }
    }

    pub fn modal(self) -> ModalCopy {
        ModalCopy {
            locale: self.locale,
        }
    }

    pub fn common(self) -> CommonCopy {
        CommonCopy {
            locale: self.locale,
        }
    }

    pub fn selector(self) -> SelectorCopy {
        SelectorCopy {
            locale: self.locale,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct StatusCopy {
    locale: Locale,
}

impl StatusCopy {
    pub fn waiting_snapshot_badge(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "WAITING SNAPSHOT",
            Locale::ZhCn => "等待快照",
        }
    }

    pub fn waiting_snapshot_message(self, narrow: bool) -> &'static str {
        match (self.locale, narrow) {
            (Locale::EnUs, true) => " Waiting for first snapshot ",
            (Locale::EnUs, false) => {
                " Waiting for /instances and the first instance snapshot before showing live data "
            }
            (Locale::ZhCn, true) => " 等待首个快照 ",
            (Locale::ZhCn, false) => " 等待 /instances 与实例首个快照后再显示实时数据 ",
        }
    }

    pub fn snapshot_failed_badge(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "SNAPSHOT FAILED",
            Locale::ZhCn => "快照失败",
        }
    }

    pub fn retry_status(self, retry_count: u32, retry_in_ms: u64) -> String {
        match self.locale {
            Locale::EnUs => format!(" retry {} in {}ms ", retry_count, retry_in_ms),
            Locale::ZhCn => format!(" 第 {} 次重试，{}ms 后执行 ", retry_count, retry_in_ms),
        }
    }

    pub fn focus_status(self, focus: &str) -> String {
        match self.locale {
            Locale::EnUs => format!(" Focus {} ", focus),
            Locale::ZhCn => format!(" 焦点 {} ", focus),
        }
    }

    pub fn pending_status(self, count: usize) -> String {
        match self.locale {
            Locale::EnUs => format!(" Pending {} ", count),
            Locale::ZhCn => format!(" 待处理 {} ", count),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BootstrapCopy {
    locale: Locale,
}

impl BootstrapCopy {
    pub fn pending_title(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Initial snapshot pending",
            Locale::ZhCn => "首个快照未就绪",
        }
    }

    pub fn pending_detail(self) -> &'static str {
        match self.locale {
            Locale::EnUs => {
                "Waiting for /instances and /instances/{symbol}/runtime/snapshot before showing live data."
            }
            Locale::ZhCn => {
                "等待 /instances 与 /instances/{symbol}/runtime/snapshot 后再显示实时数据。"
            }
        }
    }

    pub fn pending_actions_disabled(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "p/r/c/f/s are disabled.",
            Locale::ZhCn => "p/r/c/f/s 当前不可用。",
        }
    }

    pub fn failed_title(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Initial snapshot failed",
            Locale::ZhCn => "首个快照获取失败",
        }
    }

    pub fn failed_retry(self, retry_count: u32, retry_in_ms: u64) -> String {
        match self.locale {
            Locale::EnUs => format!("Retry {retry_count} in {retry_in_ms}ms."),
            Locale::ZhCn => format!("{retry_in_ms}ms 后进行第 {retry_count} 次重试。"),
        }
    }

    pub fn error_line(self, last_error: &str) -> String {
        match self.locale {
            Locale::EnUs => format!("Error: {last_error}"),
            Locale::ZhCn => format!("错误：{last_error}"),
        }
    }

    pub fn failed_actions_disabled(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "p/r/c/f/s stay disabled until retry completes.",
            Locale::ZhCn => "重试完成前，p/r/c/f/s 仍不可用。",
        }
    }

    pub fn panel_line(self, title: &str) -> String {
        match self.locale {
            Locale::EnUs => format!("Panel: {title}"),
            Locale::ZhCn => format!("面板：{title}"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DashboardCopy {
    locale: Locale,
}

impl DashboardCopy {
    pub fn execution_focus_title(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Execution Focus",
            Locale::ZhCn => "执行概览",
        }
    }

    pub fn open_orders_title(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Open Orders",
            Locale::ZhCn => "挂单概览",
        }
    }

    pub fn exchange_orders_title(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Exchange Orders",
            Locale::ZhCn => "交易所挂单",
        }
    }

    pub fn recent_fills_title(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Recent Fills",
            Locale::ZhCn => "最近成交",
        }
    }

    pub fn risk_alerts_title(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Risk + Alerts",
            Locale::ZhCn => "风险与告警",
        }
    }

    pub fn market_health_title(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Market + Health",
            Locale::ZhCn => "行情与健康",
        }
    }

    pub fn command_timeline_title(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Command Timeline",
            Locale::ZhCn => "命令时间线",
        }
    }

    pub fn pos_short_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Pos  ",
            Locale::ZhCn => "仓位  ",
        }
    }

    pub fn unrealized_short_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "   U  ",
            Locale::ZhCn => "   浮盈  ",
        }
    }

    pub fn realized_short_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "   R  ",
            Locale::ZhCn => "   已实  ",
        }
    }

    pub fn position_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Position  ",
            Locale::ZhCn => "仓位  ",
        }
    }

    pub fn unrealized_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "U-PnL  ",
            Locale::ZhCn => "浮盈亏  ",
        }
    }

    pub fn realized_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "   R-PnL  ",
            Locale::ZhCn => "   已实现  ",
        }
    }

    pub fn exchange_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Exchange  ",
            Locale::ZhCn => "交易所  ",
        }
    }

    pub fn pending_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "   Pending  ",
            Locale::ZhCn => "   待处理  ",
        }
    }

    pub fn health_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "   Health  ",
            Locale::ZhCn => "   健康  ",
        }
    }

    pub fn health_line_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Health  ",
            Locale::ZhCn => "健康  ",
        }
    }

    pub fn side_header(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Side",
            Locale::ZhCn => "方向",
        }
    }

    pub fn price_header(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Price",
            Locale::ZhCn => "价格",
        }
    }

    pub fn qty_header(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Qty",
            Locale::ZhCn => "数量",
        }
    }

    pub fn status_header(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Status",
            Locale::ZhCn => "状态",
        }
    }

    pub fn pnl_label(self, pnl: &str) -> String {
        match self.locale {
            Locale::EnUs => format!("  pnl {pnl}"),
            Locale::ZhCn => format!("  盈亏 {pnl}"),
        }
    }

    pub fn command_ref_label(self, command_ref: &str) -> String {
        match self.locale {
            Locale::EnUs => format!("cmd {command_ref}"),
            Locale::ZhCn => format!("命令 {command_ref}"),
        }
    }

    pub fn no_fills(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "No fills yet",
            Locale::ZhCn => "暂无成交",
        }
    }

    pub fn level_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Level  ",
            Locale::ZhCn => "级别  ",
        }
    }

    pub fn breaker_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "   Breaker  ",
            Locale::ZhCn => "   熔断  ",
        }
    }

    pub fn notional_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Notional  ",
            Locale::ZhCn => "名义仓位  ",
        }
    }

    pub fn stop_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "   Stop  ",
            Locale::ZhCn => "   止损  ",
        }
    }

    pub fn alert_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Alert  ",
            Locale::ZhCn => "告警  ",
        }
    }

    pub fn risk_level_label(self, level: RiskLevel) -> &'static str {
        match (self.locale, level) {
            (Locale::EnUs, RiskLevel::Ok) => "Ok",
            (Locale::EnUs, RiskLevel::Watch) => "Watch",
            (Locale::EnUs, RiskLevel::Warning) => "Warning",
            (Locale::EnUs, RiskLevel::Danger) => "Danger",
            (Locale::ZhCn, RiskLevel::Ok) => "正常",
            (Locale::ZhCn, RiskLevel::Watch) => "观察",
            (Locale::ZhCn, RiskLevel::Warning) => "警告",
            (Locale::ZhCn, RiskLevel::Danger) => "危险",
        }
    }

    pub fn last_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Last  ",
            Locale::ZhCn => "最新  ",
        }
    }

    pub fn mark_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "   Mark  ",
            Locale::ZhCn => "   标记  ",
        }
    }

    pub fn service_ws_short_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Svc WS  ",
            Locale::ZhCn => "服务WS  ",
        }
    }

    pub fn market_ws_short_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "   Mkt WS  ",
            Locale::ZhCn => "   行情WS  ",
        }
    }

    pub fn service_ws_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Service WS  ",
            Locale::ZhCn => "服务 WS  ",
        }
    }

    pub fn market_ws_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "   Market WS  ",
            Locale::ZhCn => "   行情 WS  ",
        }
    }

    pub fn no_active_alerts(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "No active alerts.",
            Locale::ZhCn => "当前没有活动告警。",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct GridCopy {
    locale: Locale,
}

impl GridCopy {
    pub fn active_grid_levels_title(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Active Grid Levels",
            Locale::ZhCn => "活动网格层",
        }
    }

    pub fn strategy_orders_title(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Strategy Orders",
            Locale::ZhCn => "策略订单",
        }
    }

    pub fn grid_summary_title(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Grid Summary",
            Locale::ZhCn => "网格概览",
        }
    }

    pub fn operator_notes_title(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Operator Notes",
            Locale::ZhCn => "操作备注",
        }
    }

    pub fn strategy_header(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Strategy",
            Locale::ZhCn => "策略",
        }
    }

    pub fn placement_header(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Placement",
            Locale::ZhCn => "落单",
        }
    }

    pub fn status_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Status  ",
            Locale::ZhCn => "状态  ",
        }
    }

    pub fn lower_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Lower  ",
            Locale::ZhCn => "下界  ",
        }
    }

    pub fn upper_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "   Upper  ",
            Locale::ZhCn => "   上界  ",
        }
    }

    pub fn center_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Center  ",
            Locale::ZhCn => "中心  ",
        }
    }

    pub fn span_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "   Span  ",
            Locale::ZhCn => "   跨度  ",
        }
    }

    pub fn active_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Active  ",
            Locale::ZhCn => "活动层  ",
        }
    }

    pub fn occupied_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "   Occupied  ",
            Locale::ZhCn => "   占用层  ",
        }
    }

    pub fn pending_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "   Pending  ",
            Locale::ZhCn => "   待处理  ",
        }
    }

    pub fn bias_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "   Bias  ",
            Locale::ZhCn => "   偏向  ",
        }
    }

    pub fn current_price_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Current Price  ",
            Locale::ZhCn => "当前价格  ",
        }
    }

    pub fn session_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Session  ",
            Locale::ZhCn => "时段  ",
        }
    }

    pub fn health_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Health  ",
            Locale::ZhCn => "健康  ",
        }
    }

    pub fn breaker_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Breaker  ",
            Locale::ZhCn => "熔断  ",
        }
    }

    pub fn aligned_message(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Grid levels are aligned with the current strategy state.",
            Locale::ZhCn => "当前网格层级与策略状态一致。",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MarketCopy {
    locale: Locale,
}

impl MarketCopy {
    pub fn tape_title(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Tape",
            Locale::ZhCn => "价格",
        }
    }

    pub fn connectivity_title(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Connectivity",
            Locale::ZhCn => "连接",
        }
    }

    pub fn runtime_title(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Runtime",
            Locale::ZhCn => "运行时",
        }
    }

    pub fn last_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Last  ",
            Locale::ZhCn => "最新  ",
        }
    }

    pub fn mark_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Mark  ",
            Locale::ZhCn => "标记  ",
        }
    }

    pub fn basis_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Basis  ",
            Locale::ZhCn => "基差  ",
        }
    }

    pub fn service_ws_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Service WS  ",
            Locale::ZhCn => "服务 WS  ",
        }
    }

    pub fn http_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "   HTTP  ",
            Locale::ZhCn => "   HTTP  ",
        }
    }

    pub fn market_ws_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Market WS  ",
            Locale::ZhCn => "行情 WS  ",
        }
    }

    pub fn user_ws_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "   User WS  ",
            Locale::ZhCn => "   用户 WS  ",
        }
    }

    pub fn stale_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Stale  ",
            Locale::ZhCn => "滞后  ",
        }
    }

    pub fn retry_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "   Retry  ",
            Locale::ZhCn => "   重试  ",
        }
    }

    pub fn market_backoff_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Market Backoff  ",
            Locale::ZhCn => "行情退避  ",
        }
    }

    pub fn mode_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Mode  ",
            Locale::ZhCn => "模式  ",
        }
    }

    pub fn session_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Session  ",
            Locale::ZhCn => "时段  ",
        }
    }

    pub fn heartbeat_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Heartbeat  ",
            Locale::ZhCn => "心跳  ",
        }
    }

    pub fn strategy_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Strategy  ",
            Locale::ZhCn => "策略  ",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct InstancesCopy {
    locale: Locale,
}

impl InstancesCopy {
    pub fn title(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Instances",
            Locale::ZhCn => "实例列表",
        }
    }

    pub fn summary_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Env",
            Locale::ZhCn => "环境",
        }
    }

    pub fn current_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Current",
            Locale::ZhCn => "当前",
        }
    }

    pub fn default_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Default",
            Locale::ZhCn => "默认",
        }
    }

    pub fn empty(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "No instances",
            Locale::ZhCn => "暂无实例",
        }
    }

    pub fn more(self, count: usize) -> String {
        match self.locale {
            Locale::EnUs => format!("+{count} more"),
            Locale::ZhCn => format!("还有 {count} 项"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct EventsCopy {
    locale: Locale,
}

impl EventsCopy {
    pub fn fills_panel_title(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Fills",
            Locale::ZhCn => "成交",
        }
    }

    pub fn alerts_panel_title(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Alerts",
            Locale::ZhCn => "告警",
        }
    }

    pub fn commands_panel_title(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Commands",
            Locale::ZhCn => "命令",
        }
    }

    pub fn system_panel_title(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "System",
            Locale::ZhCn => "系统",
        }
    }

    pub fn fills_title(self, count: usize) -> String {
        match self.locale {
            Locale::EnUs => format!("Fills ({count})"),
            Locale::ZhCn => format!("成交 ({count})"),
        }
    }

    pub fn alerts_title(self, count: usize) -> String {
        match self.locale {
            Locale::EnUs => format!("Alerts ({count})"),
            Locale::ZhCn => format!("告警 ({count})"),
        }
    }

    pub fn commands_title(self, count: usize) -> String {
        match self.locale {
            Locale::EnUs => format!("Commands ({count})"),
            Locale::ZhCn => format!("命令 ({count})"),
        }
    }

    pub fn system_title(self, count: usize, pending: usize) -> String {
        match self.locale {
            Locale::EnUs => format!("System ({count}) · pending {pending}"),
            Locale::ZhCn => format!("系统 ({count}) · 待处理 {pending}"),
        }
    }

    pub fn no_fills(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "No fills",
            Locale::ZhCn => "暂无成交",
        }
    }

    pub fn no_alerts(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "No alerts",
            Locale::ZhCn => "暂无告警",
        }
    }

    pub fn no_system_events(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "No system events",
            Locale::ZhCn => "暂无系统事件",
        }
    }

    pub fn command_ref_label(self, command_ref: &str) -> String {
        match self.locale {
            Locale::EnUs => format!("cmd {command_ref}"),
            Locale::ZhCn => format!("命令 {command_ref}"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct HelpCopy {
    locale: Locale,
}

impl HelpCopy {
    pub fn shortcuts_title(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Shortcuts",
            Locale::ZhCn => "快捷键",
        }
    }

    pub fn glossary_title(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Glossary",
            Locale::ZhCn => "术语说明",
        }
    }

    pub fn focus_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Focus  ",
            Locale::ZhCn => "焦点  ",
        }
    }

    pub fn health_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Health  ",
            Locale::ZhCn => "健康  ",
        }
    }

    pub fn shortcuts_lines(self) -> [&'static str; 6] {
        match self.locale {
            Locale::EnUs => [
                "1 Dashboard   2 Grid   3 Market   4 Events   ? Help",
                "Tab / Shift-Tab moves focus between panels on the current page.",
                "[/] Cycle inst   p Pause   r Resume   l Language",
                "c Cancel all   f Flatten now   s Shutdown after flatten",
                "Enter confirms danger actions; Esc or n cancels.",
                "q / Ctrl-C exits the client only; the service keeps running.",
            ],
            Locale::ZhCn => [
                "1 概览   2 网格   3 行情   4 事件   ? 帮助",
                "Tab / Shift-Tab 在当前页面的面板之间切换焦点。",
                "[/] 切实例   p 暂停   r 恢复   l 语言",
                "c 取消全部   f 立即平仓   s 平仓后停机",
                "Enter 确认高风险操作；Esc 或 n 取消。",
                "q / Ctrl-C 只退出客户端；服务端会继续运行。",
            ],
        }
    }

    pub fn glossary_lines(self) -> [&'static str; 4] {
        match self.locale {
            Locale::EnUs => [
                "Strategy Orders: target orders produced by the strategy.",
                "Exchange Orders: shown only when live exchange data is available.",
                "If the dashboard shows an unavailable notice,",
                "the client cannot prove the strategy orders are live on the exchange.",
            ],
            Locale::ZhCn => [
                "策略订单：由策略生成的目标订单。",
                "交易所挂单：仅在存在实时交易所数据时显示。",
                "如果概览页显示不可用提示，",
                "说明客户端无法证明策略订单已经真实存在于交易所。",
            ],
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FooterCopy {
    locale: Locale,
}

impl FooterCopy {
    pub fn snapshot_pending(self, narrow: bool) -> &'static str {
        match (self.locale, narrow) {
            (Locale::EnUs, true) => " Snapshot pending | p/r/c/f/s disabled | 1-4 pages ? help ",
            (Locale::EnUs, false) => {
                " Snapshot pending | [/] inst | p/r/c/f/s disabled | 1-4 pages ? help | Tab panels "
            }
            (Locale::ZhCn, true) => " 等待快照 | p/r/c/f/s 不可用 | 1-4 切页 ? 帮助 ",
            (Locale::ZhCn, false) => {
                " 等待快照 | [/] 实例 | p/r/c/f/s 不可用 | 1-4 切页 ? 帮助 | Tab 切面板 "
            }
        }
    }

    pub fn snapshot_failed(self, narrow: bool, retry_in_ms: u64) -> String {
        match (self.locale, narrow) {
            (Locale::EnUs, true) => {
                format!(" Snapshot failed, retry in {retry_in_ms}ms | p/r/c/f/s disabled ")
            }
            (Locale::EnUs, false) => format!(
                " Snapshot failed, retry in {retry_in_ms}ms | [/] inst | p/r/c/f/s disabled | 1-4 pages ? help | Tab panels "
            ),
            (Locale::ZhCn, true) => {
                format!(" 快照失败，{retry_in_ms}ms 后重试 | p/r/c/f/s 不可用 ")
            }
            (Locale::ZhCn, false) => format!(
                " 快照失败，{retry_in_ms}ms 后重试 | [/] 实例 | p/r/c/f/s 不可用 | 1-4 切页 ? 帮助 | Tab 切面板 "
            ),
        }
    }

    pub fn ready(self, narrow: bool, focus: &str) -> String {
        match (self.locale, narrow) {
            (Locale::EnUs, true) => {
                format!(" Focus {focus} | Tab panels | p/r run | c/f/s danger | Enter/Esc ")
            }
            (Locale::EnUs, false) => format!(
                " Focus {focus} | 1-4 pages ? help | [/] inst | Tab panels | p/r run | c/f/s danger | Enter/Esc "
            ),
            (Locale::ZhCn, true) => {
                format!(" 焦点 {focus} | Tab 切面板 | p/r 运行 | c/f/s 高风险 | Enter/Esc ")
            }
            (Locale::ZhCn, false) => format!(
                " 焦点 {focus} | 1-4 切页 ? 帮助 | [/] 实例 | Tab 切面板 | p/r 运行 | c/f/s 高风险 | Enter/Esc "
            ),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ToastCopy {
    locale: Locale,
}

impl ToastCopy {
    pub fn snapshot_failed(self, error: &str) -> String {
        match self.locale {
            Locale::EnUs => format!("snapshot failed: {error}"),
            Locale::ZhCn => format!("快照获取失败：{error}"),
        }
    }

    pub fn risk_events_failed(self, error: &str) -> String {
        match self.locale {
            Locale::EnUs => format!("risk events failed: {error}"),
            Locale::ZhCn => format!("风险事件获取失败：{error}"),
        }
    }

    pub fn ws_connected(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "ws connected",
            Locale::ZhCn => "WebSocket 已连接",
        }
    }

    pub fn ws_disconnected(self, reason: &str) -> String {
        match self.locale {
            Locale::EnUs => format!("ws disconnected: {reason}"),
            Locale::ZhCn => format!("WebSocket 已断开：{reason}"),
        }
    }

    pub fn snapshot_pending_blocked(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Initial snapshot pending. Runtime actions are disabled.",
            Locale::ZhCn => "首个快照未就绪，运行时操作已禁用。",
        }
    }

    pub fn snapshot_retrying_blocked(self) -> &'static str {
        match self.locale {
            Locale::EnUs => {
                "Initial snapshot failed. Wait for retry before sending runtime actions."
            }
            Locale::ZhCn => "首个快照获取失败，请等待重试后再发送运行时操作。",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct StoreCopy {
    locale: Locale,
}

impl StoreCopy {
    pub fn ws_connected_event(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "WebSocket connected and streaming.",
            Locale::ZhCn => "WebSocket 已连接，开始流式传输。",
        }
    }

    pub fn ws_disconnected_event(self, reason: &str) -> String {
        match self.locale {
            Locale::EnUs => format!("WebSocket disconnected: {reason}"),
            Locale::ZhCn => format!("WebSocket 已断开：{reason}"),
        }
    }

    pub fn command_accepted_summary(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Service accepted command; waiting for final acknowledgement.",
            Locale::ZhCn => "服务端已接受命令，等待最终确认。",
        }
    }

    pub fn command_failed_summary(self, error: &str) -> String {
        match self.locale {
            Locale::EnUs => format!("Command request failed before ack: {error}"),
            Locale::ZhCn => format!("命令在收到服务端确认前失败：{error}"),
        }
    }

    pub fn command_failed_toast(self, error: &str) -> String {
        match self.locale {
            Locale::EnUs => format!("command failed: {error}"),
            Locale::ZhCn => format!("命令失败：{error}"),
        }
    }

    pub fn command_timed_out_summary(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "No final acknowledgement arrived within the timeout window.",
            Locale::ZhCn => "在超时窗口内没有收到最终确认。",
        }
    }

    pub fn command_timed_out_toast(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "one or more commands timed out",
            Locale::ZhCn => "一条或多条命令已超时",
        }
    }

    pub fn command_pending_summary(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Waiting for service acceptance.",
            Locale::ZhCn => "等待服务端接受命令。",
        }
    }

    pub fn recovered_pending_summary(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Recovered pending command from snapshot.",
            Locale::ZhCn => "已从快照恢复待处理命令。",
        }
    }

    pub fn recovered_accepted_summary(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Service accepted command before the client reconnected.",
            Locale::ZhCn => "客户端重连前服务端已接受该命令。",
        }
    }

    pub fn recovered_ack_summary(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Command already acknowledged before the client reconnected.",
            Locale::ZhCn => "客户端重连前命令已确认完成。",
        }
    }

    pub fn recovered_failed_summary(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Command already failed before the client reconnected.",
            Locale::ZhCn => "客户端重连前命令已失败。",
        }
    }

    pub fn recovered_timed_out_summary(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Command timed out before the client reconnected.",
            Locale::ZhCn => "客户端重连前命令已超时。",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SelectorCopy {
    locale: Locale,
}

impl SelectorCopy {
    pub fn long_inventory(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "long inventory",
            Locale::ZhCn => "多头库存",
        }
    }

    pub fn short_inventory(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "short inventory",
            Locale::ZhCn => "空头库存",
        }
    }

    pub fn flat_inventory(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "flat inventory",
            Locale::ZhCn => "空仓",
        }
    }

    pub fn market_status(self, is_up: bool) -> &'static str {
        match (self.locale, is_up) {
            (Locale::EnUs, true) => "UP",
            (Locale::EnUs, false) => "DOWN",
            (Locale::ZhCn, true) => "在线",
            (Locale::ZhCn, false) => "离线",
        }
    }

    pub fn optional_market_status(self, is_up: Option<bool>) -> &'static str {
        match (self.locale, is_up) {
            (Locale::EnUs, Some(true)) => "UP",
            (Locale::EnUs, Some(false)) => "DOWN",
            (Locale::EnUs, None) => "N/A",
            (Locale::ZhCn, Some(true)) => "在线",
            (Locale::ZhCn, Some(false)) => "离线",
            (Locale::ZhCn, None) => "无",
        }
    }

    pub fn service_reconnecting_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "SERVICE RECONNECTING",
            Locale::ZhCn => "服务重连中",
        }
    }

    pub fn service_reconnecting_detail(self, attempt: u32, backoff_ms: u64) -> String {
        match self.locale {
            Locale::EnUs => format!("service ws retry {attempt} in {backoff_ms}ms"),
            Locale::ZhCn => format!("服务 WS 第 {attempt} 次重试，{backoff_ms}ms 后执行"),
        }
    }

    pub fn service_reconnecting_hint(self) -> &'static str {
        match self.locale {
            Locale::EnUs => {
                "Service control-plane WebSocket is down. Wait for the client stream to recover."
            }
            Locale::ZhCn => "服务控制面的 WebSocket 已断开，请等待客户端流恢复。",
        }
    }

    pub fn market_reconnecting_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "MARKET RECONNECTING",
            Locale::ZhCn => "行情重连中",
        }
    }

    pub fn market_reconnecting_detail(self, backoff_ms: u64) -> String {
        match self.locale {
            Locale::EnUs => format!("binance ws retry in {backoff_ms}ms"),
            Locale::ZhCn => format!("Binance WS 将在 {backoff_ms}ms 后重试"),
        }
    }

    pub fn market_reconnecting_hint(self) -> &'static str {
        match self.locale {
            Locale::EnUs => {
                "Service is online, but Binance market stream is reconnecting. Treat market data as stale."
            }
            Locale::ZhCn => "服务在线，但 Binance 行情流正在重连。请按滞后数据处理行情。",
        }
    }

    pub fn stale_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "STALE",
            Locale::ZhCn => "滞后",
        }
    }

    pub fn stale_detail(self, stale_age_ms: u64) -> String {
        match self.locale {
            Locale::EnUs => format!("feed lag {stale_age_ms}ms"),
            Locale::ZhCn => format!("行情延迟 {stale_age_ms}ms"),
        }
    }

    pub fn stale_hint(self) -> &'static str {
        match self.locale {
            Locale::EnUs => {
                "Heartbeat is alive but market data is not advancing. Recheck before trading."
            }
            Locale::ZhCn => "心跳仍在，但行情数据没有推进。交易前请重新确认。",
        }
    }

    pub fn degraded_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "DEGRADED",
            Locale::ZhCn => "降级",
        }
    }

    pub fn degraded_detail(
        self,
        http_available: bool,
        stale_age_ms: u64,
        user_stream_connected: Option<bool>,
    ) -> String {
        match self.locale {
            Locale::EnUs => format!(
                "http {} / stale {}ms / user {}",
                self.status_word(http_available),
                stale_age_ms,
                self.optional_status_word(user_stream_connected)
            ),
            Locale::ZhCn => format!(
                "http {} / 滞后 {}ms / 用户 {}",
                self.status_word(http_available),
                stale_age_ms,
                self.optional_status_word(user_stream_connected)
            ),
        }
    }

    pub fn degraded_hint(self) -> &'static str {
        match self.locale {
            Locale::EnUs => {
                "Connection is usable but lagging. Avoid risky commands until it settles."
            }
            Locale::ZhCn => "连接可用但存在延迟，在稳定前避免发送高风险命令。",
        }
    }

    pub fn healthy_label(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "HEALTHY",
            Locale::ZhCn => "健康",
        }
    }

    pub fn healthy_detail(self, stale_age_ms: u64) -> String {
        match self.locale {
            Locale::EnUs => format!("stale {stale_age_ms}ms"),
            Locale::ZhCn => format!("滞后 {stale_age_ms}ms"),
        }
    }

    pub fn healthy_hint(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Service and Binance streams are both healthy.",
            Locale::ZhCn => "服务与 Binance 行情流都处于健康状态。",
        }
    }

    pub fn dashboard_service_retry_detail(self, backoff_ms: u64) -> String {
        match self.locale {
            Locale::EnUs => format!("svc retry {backoff_ms}ms"),
            Locale::ZhCn => format!("服务重试 {backoff_ms}ms"),
        }
    }

    pub fn dashboard_market_retry_detail(self, backoff_ms: u64) -> String {
        match self.locale {
            Locale::EnUs => format!("mkt retry {backoff_ms}ms"),
            Locale::ZhCn => format!("行情重试 {backoff_ms}ms"),
        }
    }

    pub fn dashboard_http_down(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "http down",
            Locale::ZhCn => "http 离线",
        }
    }

    pub fn dashboard_user_down(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "user down",
            Locale::ZhCn => "用户流离线",
        }
    }

    pub fn status_word(self, is_up: bool) -> &'static str {
        match (self.locale, is_up) {
            (Locale::EnUs, true) => "up",
            (Locale::EnUs, false) => "down",
            (Locale::ZhCn, true) => "在线",
            (Locale::ZhCn, false) => "离线",
        }
    }

    pub fn optional_status_word(self, is_up: Option<bool>) -> &'static str {
        match (self.locale, is_up) {
            (Locale::EnUs, Some(true)) => "up",
            (Locale::EnUs, Some(false)) => "down",
            (Locale::EnUs, None) => "n/a",
            (Locale::ZhCn, Some(true)) => "在线",
            (Locale::ZhCn, Some(false)) => "离线",
            (Locale::ZhCn, None) => "无",
        }
    }

    pub fn risk_action_hint(self, code: &str) -> &'static str {
        match (self.locale, code) {
            (Locale::EnUs, "MAX_POSITION_EXCEEDED") => {
                "Reduce exposure before placing new grid orders."
            }
            (Locale::ZhCn, "MAX_POSITION_EXCEEDED") => "新网格订单生效前先降低风险敞口。",
            (Locale::EnUs, "STOP_LOSS_TRIGGERED") => "Reduce exposure before resuming the grid.",
            (Locale::ZhCn, "STOP_LOSS_TRIGGERED") => "恢复网格前先降低风险敞口。",
            (Locale::EnUs, "DAILY_LOSS_LIMIT_BREACHED") => {
                "Pause new orders and review daily loss limits."
            }
            (Locale::ZhCn, "DAILY_LOSS_LIMIT_BREACHED") => "暂停新订单，并复核当日亏损限制。",
            (Locale::EnUs, "BREAKER_RELEASED") => {
                "Breaker released. Verify conditions before resuming."
            }
            (Locale::ZhCn, "BREAKER_RELEASED") => "熔断已解除，恢复前请先确认条件。",
            (Locale::EnUs, _) => "Review the risk configuration before resuming.",
            (Locale::ZhCn, _) => "恢复前请先检查风险配置。",
        }
    }

    pub fn strategy_status_label(self, status: StrategyStatus) -> &'static str {
        match (self.locale, status) {
            (Locale::EnUs, StrategyStatus::WaitingMarketPrice) => "WAITING PRICE",
            (Locale::EnUs, StrategyStatus::WaitingRangeEntry) => "WAITING RANGE",
            (Locale::EnUs, StrategyStatus::Active) => "ACTIVE",
            (Locale::EnUs, StrategyStatus::Occupied) => "OCCUPIED",
            (Locale::EnUs, StrategyStatus::PendingRebuild) => "PENDING REBUILD",
            (Locale::ZhCn, StrategyStatus::WaitingMarketPrice) => "等待价格",
            (Locale::ZhCn, StrategyStatus::WaitingRangeEntry) => "等待入区",
            (Locale::ZhCn, StrategyStatus::Active) => "激活",
            (Locale::ZhCn, StrategyStatus::Occupied) => "占用",
            (Locale::ZhCn, StrategyStatus::PendingRebuild) => "待重建",
        }
    }

    pub fn grid_level_state_label(self, state: GridLevelState) -> &'static str {
        match (self.locale, state) {
            (Locale::EnUs, GridLevelState::Active) => "ACTIVE",
            (Locale::EnUs, GridLevelState::Occupied) => "OCCUPIED",
            (Locale::EnUs, GridLevelState::PendingRebuild) => "PENDING REBUILD",
            (Locale::ZhCn, GridLevelState::Active) => "激活",
            (Locale::ZhCn, GridLevelState::Occupied) => "占用",
            (Locale::ZhCn, GridLevelState::PendingRebuild) => "待重建",
        }
    }

    pub fn command_label(self, command: CommandType) -> &'static str {
        match (self.locale, command) {
            (Locale::EnUs, CommandType::Pause) => "PAUSE",
            (Locale::EnUs, CommandType::Resume) => "RESUME",
            (Locale::EnUs, CommandType::CancelAll) => "CANCEL ALL",
            (Locale::EnUs, CommandType::FlattenNow) => "FLATTEN NOW",
            (Locale::EnUs, CommandType::ShutdownAfterFlatten) => "SHUTDOWN",
            (Locale::ZhCn, CommandType::Pause) => "暂停",
            (Locale::ZhCn, CommandType::Resume) => "恢复",
            (Locale::ZhCn, CommandType::CancelAll) => "取消全部",
            (Locale::ZhCn, CommandType::FlattenNow) => "立即平仓",
            (Locale::ZhCn, CommandType::ShutdownAfterFlatten) => "平仓后停机",
        }
    }

    pub fn stage_label(self, stage: CommandTimelineStage) -> &'static str {
        match (self.locale, stage) {
            (Locale::EnUs, CommandTimelineStage::Pending) => "PENDING",
            (Locale::EnUs, CommandTimelineStage::Accepted) => "ACCEPTED",
            (Locale::EnUs, CommandTimelineStage::Ack) => "ACK",
            (Locale::EnUs, CommandTimelineStage::Failed) => "FAILED",
            (Locale::EnUs, CommandTimelineStage::TimedOut) => "TIMED OUT",
            (Locale::ZhCn, CommandTimelineStage::Pending) => "待处理",
            (Locale::ZhCn, CommandTimelineStage::Accepted) => "已接受",
            (Locale::ZhCn, CommandTimelineStage::Ack) => "已确认",
            (Locale::ZhCn, CommandTimelineStage::Failed) => "失败",
            (Locale::ZhCn, CommandTimelineStage::TimedOut) => "超时",
        }
    }

    pub fn command_timing(
        self,
        requested_at: &str,
        accepted_at: Option<&str>,
        finished_at: Option<&str>,
    ) -> String {
        match (self.locale, accepted_at, finished_at) {
            (Locale::EnUs, Some(accepted_at), Some(finished_at)) => {
                format!("req {requested_at} -> acc {accepted_at} -> end {finished_at}")
            }
            (Locale::EnUs, Some(accepted_at), None) => {
                format!("req {requested_at} -> acc {accepted_at}")
            }
            (Locale::EnUs, None, Some(finished_at)) => {
                format!("req {requested_at} -> end {finished_at}")
            }
            (Locale::EnUs, None, None) => format!("req {requested_at}"),
            (Locale::ZhCn, Some(accepted_at), Some(finished_at)) => {
                format!("请求 {requested_at} -> 接受 {accepted_at} -> 完成 {finished_at}")
            }
            (Locale::ZhCn, Some(accepted_at), None) => {
                format!("请求 {requested_at} -> 接受 {accepted_at}")
            }
            (Locale::ZhCn, None, Some(finished_at)) => {
                format!("请求 {requested_at} -> 完成 {finished_at}")
            }
            (Locale::ZhCn, None, None) => format!("请求 {requested_at}"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ModalCopy {
    locale: Locale,
}

impl ModalCopy {
    pub fn confirm(self, command: CommandType) -> (&'static str, &'static str, &'static str) {
        match (self.locale, command) {
            (Locale::EnUs, CommandType::CancelAll) => (
                "Confirm Cancel All",
                "All resting grid orders will be cancelled immediately.",
                "Open position remains live; verify inventory before sending more orders.",
            ),
            (Locale::ZhCn, CommandType::CancelAll) => (
                "确认取消全部",
                "所有网格挂单都会被立即取消。",
                "持仓仍然保留；发送新订单前请先确认库存风险。",
            ),
            (Locale::EnUs, CommandType::FlattenNow) => (
                "Confirm Flatten Now",
                "The client will request an immediate flatten of current inventory.",
                "This can cross the spread, realize slippage, and may fail if connectivity is degraded.",
            ),
            (Locale::ZhCn, CommandType::FlattenNow) => (
                "确认立即平仓",
                "客户端会请求立即平掉当前持仓。",
                "这可能吃到价差、产生滑点，并在连接退化时失败。",
            ),
            (Locale::EnUs, CommandType::ShutdownAfterFlatten) => (
                "Confirm Shutdown After Flatten",
                "The strategy will flatten first and then stay paused for operator review.",
                "Use this when you want a controlled stop, not a quick resume cycle.",
            ),
            (Locale::ZhCn, CommandType::ShutdownAfterFlatten) => (
                "确认平仓后停机",
                "策略会先平仓，然后保持暂停，等待人工检查。",
                "适用于需要受控停机，而不是快速恢复的场景。",
            ),
            (Locale::EnUs, CommandType::Pause) => (
                "Confirm Pause",
                "The strategy will stop placing new orders after the service acknowledges it.",
                "Existing orders may remain on the book until a later cancel request.",
            ),
            (Locale::ZhCn, CommandType::Pause) => (
                "确认暂停",
                "服务确认后，策略将停止提交新订单。",
                "现有挂单可能仍留在簿上，直到后续发送取消请求。",
            ),
            (Locale::EnUs, CommandType::Resume) => (
                "Confirm Resume",
                "The strategy will resume normal order management after acknowledgement.",
                "Confirm market health first if the client is showing stale or reconnecting status.",
            ),
            (Locale::ZhCn, CommandType::Resume) => (
                "确认恢复",
                "收到确认后，策略将恢复正常订单管理。",
                "如果客户端显示滞后或重连状态，请先确认行情健康。",
            ),
        }
    }

    pub fn confirm_hint(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "Press Enter to confirm, or Esc / n to cancel.",
            Locale::ZhCn => "按 Enter 确认，或按 Esc / n 取消。",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CommonCopy {
    locale: Locale,
}

impl CommonCopy {
    pub fn not_available(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "N/A",
            Locale::ZhCn => "无",
        }
    }

    pub fn none_value(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "None",
            Locale::ZhCn => "无",
        }
    }

    pub fn on(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "ON",
            Locale::ZhCn => "开",
        }
    }

    pub fn off(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "OFF",
            Locale::ZhCn => "关",
        }
    }

    pub fn up(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "UP",
            Locale::ZhCn => "在线",
        }
    }

    pub fn down(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "DOWN",
            Locale::ZhCn => "离线",
        }
    }

    pub fn exchange_orders_mirror_lines(self) -> [&'static str; 3] {
        match self.locale {
            Locale::EnUs => [
                "Current mode exposes strategy-order mirrors only.",
                "Live exchange-order queries are not enabled.",
                "Open Grid to inspect strategy orders.",
            ],
            Locale::ZhCn => [
                "当前模式只暴露策略订单镜像。",
                "尚未启用实时交易所挂单查询。",
                "可切到网格页查看策略订单。",
            ],
        }
    }

    pub fn exchange_orders_unavailable_lines(self) -> [&'static str; 2] {
        match self.locale {
            Locale::EnUs => [
                "Current mode does not provide exchange-order data.",
                "Check the runtime mode and execution adapter.",
            ],
            Locale::ZhCn => [
                "当前模式不提供交易所挂单数据。",
                "请检查运行模式和执行适配器。",
            ],
        }
    }

    pub fn strategy_state_label(self, state: &str) -> String {
        match (self.locale, state) {
            (Locale::EnUs, "ACTIVE") => "Active".into(),
            (Locale::EnUs, "OCCUPIED") => "Occupied".into(),
            (Locale::EnUs, "PENDING REBUILD") => "Rebuild".into(),
            (Locale::ZhCn, "ACTIVE") => "激活".into(),
            (Locale::ZhCn, "OCCUPIED") => "占用".into(),
            (Locale::ZhCn, "PENDING REBUILD") => "重建".into(),
            (_, other) => other.into(),
        }
    }

    pub fn placement_state_label(self, state: selectors::PlacementState) -> &'static str {
        match (self.locale, state) {
            (Locale::EnUs, selectors::PlacementState::Live) => "Live",
            (Locale::EnUs, selectors::PlacementState::NotPlaced) => "Missing",
            (Locale::EnUs, selectors::PlacementState::NotExpected) => "N/A",
            (Locale::EnUs, selectors::PlacementState::Unknown) => "Unknown",
            (Locale::ZhCn, selectors::PlacementState::Live) => "在线",
            (Locale::ZhCn, selectors::PlacementState::NotPlaced) => "缺失",
            (Locale::ZhCn, selectors::PlacementState::NotExpected) => "N/A",
            (Locale::ZhCn, selectors::PlacementState::Unknown) => "未知",
        }
    }

    pub fn no_recent_commands(self) -> &'static str {
        match self.locale {
            Locale::EnUs => "No recent commands",
            Locale::ZhCn => "暂无命令",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Locale;

    #[test]
    fn parses_supported_env_values() {
        assert_eq!(Locale::from_env_value("en-US"), Some(Locale::EnUs));
        assert_eq!(Locale::from_env_value("zh-CN"), Some(Locale::ZhCn));
    }

    #[test]
    fn rejects_unknown_env_values() {
        assert_eq!(Locale::from_env_value("fr-FR"), None);
    }

    #[test]
    fn toggle_switches_between_two_supported_locales() {
        assert_eq!(Locale::EnUs.toggle(), Locale::ZhCn);
        assert_eq!(Locale::ZhCn.toggle(), Locale::EnUs);
    }
}
