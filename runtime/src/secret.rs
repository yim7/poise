//! 私钥与地址。
//!
//! 私钥只在启动时从启动文件读进来，之后只用于签名。它不实现 `Display`、不实现
//! `Serialize`，`Debug` 也不打印内容：这样它进不了日志、错误信息、调试输出、
//! 数据库和接口响应（设计第 2.0 节、B47）。
//!
//! 地址不是秘密，可以显示：界面和错误信息里用它让用户核对「推导出的 API 钱包
//! 地址是不是自己授权的那个」。

use std::fmt;

use k256::ecdsa::SigningKey;
use sha3::{Digest, Keccak256};

/// 以太坊风格地址：20 字节，显示为小写 `0x` 加 40 个十六进制字符。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Address([u8; 20]);

impl Address {
    pub fn parse(value: &str) -> Result<Self, SecretError> {
        let hex = value
            .trim()
            .strip_prefix("0x")
            .ok_or(SecretError::MalformedAddress)?;
        if hex.len() != 40 || !hex.chars().all(|digit| digit.is_ascii_hexdigit()) {
            return Err(SecretError::MalformedAddress);
        }
        let bytes = hex::decode(hex).map_err(|_| SecretError::MalformedAddress)?;
        let mut address = [0_u8; 20];
        address.copy_from_slice(&bytes);
        Ok(Self(address))
    }

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }
}

impl fmt::Display for Address {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("0x")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    #[error("the private key must be `0x` followed by 64 hexadecimal characters")]
    MalformedPrivateKey,
    #[error("the private key is not a valid secp256k1 scalar")]
    InvalidPrivateKey,
    #[error("the address must be `0x` followed by 40 hexadecimal characters")]
    MalformedAddress,
}

/// 一个 secp256k1 私钥。
///
/// `Debug` 不显示内容；没有 `Display`、`Serialize`，也没有任何返回私钥本身的方法。
///
/// `Clone` 只用于把同一把钥匙交给需要各自持有的组件（启动校验读到的文件、
/// 交易所客户端）。克隆不改变「不进日志、不进数据库」的约束。
#[derive(Clone)]
pub struct SecretKey(SigningKey);

impl SecretKey {
    /// 解析 `0x` 加 64 个十六进制字符的私钥。
    ///
    /// 错误信息里只有格式说明，没有输入内容。
    pub fn parse(value: &str) -> Result<Self, SecretError> {
        let hex = value
            .trim()
            .strip_prefix("0x")
            .ok_or(SecretError::MalformedPrivateKey)?;
        if hex.len() != 64 || !hex.chars().all(|digit| digit.is_ascii_hexdigit()) {
            return Err(SecretError::MalformedPrivateKey);
        }
        let bytes = hex::decode(hex).map_err(|_| SecretError::MalformedPrivateKey)?;
        let signing_key =
            SigningKey::from_slice(&bytes).map_err(|_| SecretError::InvalidPrivateKey)?;
        Ok(Self(signing_key))
    }

    /// 由私钥推出的地址。Hyperliquid 的 API 钱包地址就是这个。
    pub fn address(&self) -> Address {
        let verifying_key = self.0.verifying_key();
        let encoded = verifying_key.to_encoded_point(false);
        address_of_public_key(encoded.as_bytes())
    }

    /// 签名用的密钥。只允许交给签名代码，不要打印、不要序列化。
    pub fn signing_key(&self) -> &SigningKey {
        &self.0
    }
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretKey(<redacted>)")
    }
}

/// 未压缩公钥（65 字节，`0x04` 开头）的 Keccak-256 摘要末 20 字节。
fn address_of_public_key(encoded: &[u8]) -> Address {
    debug_assert_eq!(encoded.len(), 65, "expected an uncompressed public key");
    let mut hasher = Keccak256::new();
    hasher.update(&encoded[1..]);
    let digest = hasher.finalize();
    let mut address = [0_u8; 20];
    address.copy_from_slice(&digest[12..]);
    Address(address)
}

#[cfg(test)]
mod tests {
    use super::{Address, SecretError, SecretKey};

    /// 私钥 0x…01 的公开测试向量。
    const ONE: &str = "0x0000000000000000000000000000000000000000000000000000000000000001";
    const ONE_ADDRESS: &str = "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf";

    /// 私钥 0x…02 的公开测试向量。
    const TWO: &str = "0x0000000000000000000000000000000000000000000000000000000000000002";
    const TWO_ADDRESS: &str = "0x2b5ad5c4795c026514f8317c7a215e218dccd6cf";

    #[test]
    fn derives_the_published_address_for_a_known_private_key() {
        assert_eq!(
            SecretKey::parse(ONE).unwrap().address().to_string(),
            ONE_ADDRESS
        );
        assert_eq!(
            SecretKey::parse(TWO).unwrap().address().to_string(),
            TWO_ADDRESS
        );
    }

    #[test]
    fn rejects_malformed_private_keys_without_echoing_them() {
        for value in [
            "",
            "   ",
            "7e5f4552091a69125d5dfcb7b8c2659029395bdf",
            &format!("0x{}", "1".repeat(63)),
            &format!("0x{}", "1".repeat(65)),
            &format!("0x{}", "z".repeat(64)),
            // 超出 secp256k1 群阶。
            "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        ] {
            let error = SecretKey::parse(value).unwrap_err();
            assert!(
                matches!(
                    error,
                    SecretError::MalformedPrivateKey | SecretError::InvalidPrivateKey
                ),
                "unexpected error for a malformed key: {error}"
            );
            assert!(!error.to_string().contains("7e5f4552"));
        }
    }

    #[test]
    fn debug_never_shows_the_private_key() {
        let key = SecretKey::parse(ONE).unwrap();

        let debug = format!("{key:?}");

        assert_eq!(debug, "SecretKey(<redacted>)");
        assert!(!debug.contains("0001"));
    }

    #[test]
    fn a_clone_is_the_same_key_and_stays_redacted() {
        let key = SecretKey::parse(ONE).unwrap();
        let copy = key.clone();

        assert_eq!(copy.address(), key.address());
        assert_eq!(format!("{copy:?}"), "SecretKey(<redacted>)");
    }

    #[test]
    fn parses_addresses_in_any_case_and_displays_them_lowercase() {
        assert_eq!(
            Address::parse("0x7E5F4552091A69125D5DFCB7B8C2659029395BDF")
                .unwrap()
                .to_string(),
            ONE_ADDRESS
        );
        assert_eq!(
            format!("{:?}", Address::parse(ONE_ADDRESS).unwrap()),
            ONE_ADDRESS
        );
        for value in ["", "7e5f4552", "0x7e5f", "0xzz"] {
            assert_eq!(
                Address::parse(value).unwrap_err(),
                SecretError::MalformedAddress
            );
        }
    }
}
