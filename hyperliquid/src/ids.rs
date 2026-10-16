//! 客户端订单 ID 与 Hyperliquid `cloid` 的互转。
//!
//! `cloid` 固定是 `0x` 加 32 个十六进制字符（16 字节）。runtime 生成的客户端订单
//! ID 就是这个形状；形状不对的 ID 交易所会直接拒绝，所以在本地就按拒绝处理，
//! 不去浪费一次请求。

use poise_strategy::order::ClientOrderId;

pub(crate) fn cloid_of(client_order_id: &ClientOrderId) -> Result<String, String> {
    let hex = client_order_id
        .as_str()
        .strip_prefix("0x")
        .ok_or_else(|| "client order id must start with `0x`".to_string())?;
    if hex.len() != 32 || !hex.chars().all(|digit| digit.is_ascii_hexdigit()) {
        return Err(
            "client order id must be `0x` followed by 32 hexadecimal characters".to_string(),
        );
    }
    Ok(format!("0x{}", hex.to_ascii_lowercase()))
}

/// 交易所给的 `cloid` 转回客户端订单 ID。形状不对时返回 `None`：
/// 那不是我们下的单，忽略即可。
pub(crate) fn client_order_id_of(cloid: &str) -> Option<ClientOrderId> {
    let hex = cloid.trim().strip_prefix("0x")?;
    if hex.len() != 32 || !hex.chars().all(|digit| digit.is_ascii_hexdigit()) {
        return None;
    }
    ClientOrderId::try_new(format!("0x{}", hex.to_ascii_lowercase())).ok()
}

#[cfg(test)]
mod tests {
    use super::{client_order_id_of, cloid_of};
    use poise_strategy::order::ClientOrderId;

    #[test]
    fn normalizes_client_order_ids_to_lowercase_cloids() {
        let id = ClientOrderId::try_new("0x1E60610F0B3D420597C88C1FED2AD5EE").unwrap();

        assert_eq!(cloid_of(&id).unwrap(), "0x1e60610f0b3d420597c88c1fed2ad5ee");
    }

    #[test]
    fn refuses_client_order_ids_hyperliquid_would_reject() {
        for value in [
            "1e60610f0b3d420597c88c1fed2ad5ee",
            "0x1e60610f0b3d420597c88c1fed2ad5e",
            "0x1e60610f0b3d420597c88c1fed2ad5eeff",
            "0xzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
            "order-1",
        ] {
            let id = ClientOrderId::try_new(value).unwrap();
            assert!(cloid_of(&id).is_err(), "`{value}` should be refused");
        }
    }

    #[test]
    fn round_trips_cloids_and_ignores_foreign_ones() {
        let id = ClientOrderId::try_new("0x1e60610f0b3d420597c88c1fed2ad5ee").unwrap();

        assert_eq!(
            client_order_id_of(&cloid_of(&id).unwrap()).unwrap(),
            id,
            "交易所返回的 cloid 要能认回本地这张单"
        );
        assert_eq!(
            client_order_id_of("0x1E60610F0B3D420597C88C1FED2AD5EE").unwrap(),
            id,
            "大小写不影响身份"
        );
        assert_eq!(client_order_id_of("0xdeadbeef"), None);
        assert_eq!(client_order_id_of(""), None);
    }
}
