//! 账户风控设置（第 7.2、7.4、7.6、11 节）：保证金率的提醒阈值与停止阈值、行情过期时间、
//! 资金费率的提醒阈值。
//!
//! 保证金是整个账户共用的，这几项也按账户设，同一账户的所有策略共用一份。存在数据库里，
//! 网页上查看和修改；库里还没有时用默认值。

use chrono::Duration;
use poise_strategy::units::Rate;
use serde::{Deserialize, Serialize};

use crate::runtime::percent;

/// 账户风控设置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountSettings {
    /// 保证金率超过它时提醒用户（默认 30%）。
    pub margin_alert_ratio: Rate,
    /// 保证金率超过它时同一账户所有策略只发、只保留只减仓单（默认 50%）。
    pub margin_stop_ratio: Rate,
    /// 行情读回来之后多久算过期：过期之后这一步不再发新单（第 7.2 节）。
    ///
    /// 一步里要核对订单、核对仓位、逐张发单，慢的时候会花掉不少时间；行情是这一步开头
    /// 读的，发单时已经旧了，按它判断「会不会立即成交」「会不会和自己的单相交」就不可靠。
    #[serde(with = "seconds")]
    pub market_data_max_age: Duration,
    /// 资金费率对本策略不利、年化超过它时提醒（默认 50%，第 7.6 节）。
    #[serde(default = "default_funding_alert")]
    pub funding_alert_annual_rate: Rate,
}

/// 资金费率的提醒阈值：年化 50%。基础利率是年化约 11%（每小时 0.00125%）。
fn default_funding_alert() -> Rate {
    Rate::from_units(50_000_000)
}

impl Default for AccountSettings {
    fn default() -> Self {
        Self {
            margin_alert_ratio: Rate::from_units(30_000_000),
            margin_stop_ratio: Rate::from_units(50_000_000),
            market_data_max_age: Duration::seconds(10),
            funding_alert_annual_rate: default_funding_alert(),
        }
    }
}

/// 设置不合理：改完再提交。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SettingsError {
    #[error("提醒阈值 {} 须大于 0、小于停止阈值 {}", percent(*.alert), percent(*.stop))]
    AlertNotBelowStop { alert: Rate, stop: Rate },
    #[error("停止阈值 {} 须小于 100%", percent(*.0))]
    StopNotBelowOne(Rate),
    #[error("行情过期时间至少 1 秒")]
    MaxAgeTooShort,
    #[error("资金费率提醒阈值须大于 0")]
    FundingAlertNotPositive(Rate),
}

impl AccountSettings {
    /// 检查设置：`0 < 提醒阈值 < 停止阈值 < 100%`，行情过期时间至少 1 秒。
    pub fn validate(&self) -> Result<(), SettingsError> {
        if self.margin_alert_ratio <= Rate::ZERO
            || self.margin_alert_ratio >= self.margin_stop_ratio
        {
            return Err(SettingsError::AlertNotBelowStop {
                alert: self.margin_alert_ratio,
                stop: self.margin_stop_ratio,
            });
        }
        if self.margin_stop_ratio >= Rate::from_units(100_000_000) {
            return Err(SettingsError::StopNotBelowOne(self.margin_stop_ratio));
        }
        if self.market_data_max_age < Duration::seconds(1) {
            return Err(SettingsError::MaxAgeTooShort);
        }
        if self.funding_alert_annual_rate <= Rate::ZERO {
            return Err(SettingsError::FundingAlertNotPositive(
                self.funding_alert_annual_rate,
            ));
        }
        Ok(())
    }
}

/// 行情过期时间按整秒存取：网页上填的就是秒数。
mod seconds {
    use chrono::Duration;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_i64(value.num_seconds())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        let seconds = i64::deserialize(deserializer)?;
        Duration::try_seconds(seconds)
            .ok_or_else(|| serde::de::Error::custom(format!("{seconds} seconds is out of range")))
    }
}

#[cfg(test)]
mod tests {
    use super::{AccountSettings, SettingsError};
    use chrono::Duration;
    use poise_strategy::units::Rate;

    fn rate(value: &str) -> Rate {
        Rate::parse(value).unwrap()
    }

    #[test]
    fn the_defaults_are_the_design_thresholds() {
        let settings = AccountSettings::default();

        assert_eq!(settings.margin_alert_ratio, rate("0.3"));
        assert_eq!(settings.margin_stop_ratio, rate("0.5"));
        assert_eq!(settings.market_data_max_age, Duration::seconds(10));
        assert_eq!(settings.funding_alert_annual_rate, rate("0.5"));
        settings.validate().unwrap();
    }

    #[test]
    fn the_alert_threshold_must_be_below_the_stop_threshold() {
        let settings = AccountSettings {
            margin_alert_ratio: rate("0.5"),
            margin_stop_ratio: rate("0.5"),
            ..AccountSettings::default()
        };

        assert!(matches!(
            settings.validate(),
            Err(SettingsError::AlertNotBelowStop { .. })
        ));
    }

    #[test]
    fn the_stop_threshold_must_be_below_one_hundred_percent() {
        let settings = AccountSettings {
            margin_stop_ratio: rate("1"),
            ..AccountSettings::default()
        };

        assert_eq!(
            settings.validate(),
            Err(SettingsError::StopNotBelowOne(rate("1")))
        );
    }

    /// 秒数超出能表示的范围：报解析错误，不能让服务端崩掉。
    #[test]
    fn an_out_of_range_max_age_is_a_parse_error() {
        let json = r#"{"margin_alert_ratio":"0.3","margin_stop_ratio":"0.5","market_data_max_age":10000000000000000}"#;

        assert!(serde_json::from_str::<AccountSettings>(json).is_err());
    }

    #[test]
    fn the_settings_are_stored_as_plain_json() {
        let json = serde_json::to_value(AccountSettings::default()).unwrap();

        assert_eq!(
            json,
            serde_json::json!({
                "margin_alert_ratio": "0.3",
                "margin_stop_ratio": "0.5",
                "market_data_max_age": 10,
                "funding_alert_annual_rate": "0.5",
            })
        );
    }

    /// 加这一项之前存下的设置照样能读，资金费率提醒阈值取默认值。
    #[test]
    fn settings_saved_before_the_funding_alert_still_load() {
        let json =
            r#"{"margin_alert_ratio":"0.3","margin_stop_ratio":"0.5","market_data_max_age":10}"#;

        let settings: AccountSettings = serde_json::from_str(json).unwrap();

        assert_eq!(settings.funding_alert_annual_rate, rate("0.5"));
    }
}
