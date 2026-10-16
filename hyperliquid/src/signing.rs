//! Hyperliquid 的 L1 动作签名。
//!
//! 两步：
//!
//! 1. **动作哈希**：`msgpack(action) ‖ nonce(8 字节大端) ‖ 0x00` 的 Keccak-256。
//!    末字节是 vault 标记，我们不通过 vault 交易，所以恒为 0。
//!    带交易所侧有效期时，vault 标记之后再补 `0x00` 与 8 字节大端的毫秒时间戳。
//! 2. **EIP-712 签名**：用 `Agent(string source,bytes32 connectionId)` 这个结构，
//!    域是 `EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)`
//!    = `("Exchange","1",1337,0x0)`；`source` 主网是 `a`、测试网是 `b`。
//!
//! 两步都必须与交易所一致，任何一处不同都会得到 `Invalid signature`。

use k256::ecdsa::SigningKey;
use poise_runtime::{Network, SecretKey};
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use sha3::{Digest, Keccak256};

/// EIP-712 域的 chainId。Hyperliquid 用 1337，与主网/测试网无关。
const DOMAIN_CHAIN_ID: u64 = 1337;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum SigningError {
    #[error("cannot encode the action for signing")]
    Encode,
    #[error("cannot sign the action")]
    Sign,
}

/// `r ‖ s ‖ v`，按交易所要求的十六进制字符串序列化。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Signature {
    r: [u8; 32],
    s: [u8; 32],
    v: u8,
}

impl Signature {
    #[cfg(test)]
    pub(crate) fn to_compact_hex(&self) -> String {
        let mut bytes = Vec::with_capacity(65);
        bytes.extend_from_slice(&self.r);
        bytes.extend_from_slice(&self.s);
        bytes.push(self.v);
        hex::encode(bytes)
    }
}

impl Serialize for Signature {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("Signature", 3)?;
        state.serialize_field("r", &format!("0x{}", hex::encode(self.r)))?;
        state.serialize_field("s", &format!("0x{}", hex::encode(self.s)))?;
        state.serialize_field("v", &self.v)?;
        state.end()
    }
}

/// 动作哈希：msgpack 编码的动作、nonce、vault 标记、可选有效期的 Keccak-256。
///
/// 字节布局与官方 SDK 的 `action_hash(action, vault_address, nonce, expires_after)` 一致：
/// vault 标记之后，`expires_after` 非空时再补一个 `0x00` 标记字节和 8 字节大端的毫秒时间戳。
/// 交易所会按同样的布局重算一遍，少一个字节都算签名不符。
pub(crate) fn action_hash<T: Serialize>(
    action: &T,
    nonce: u64,
    expires_after: Option<u64>,
) -> Result<[u8; 32], SigningError> {
    let mut payload = rmp_serde::to_vec_named(action).map_err(|_| SigningError::Encode)?;
    payload.extend_from_slice(&nonce.to_be_bytes());
    // vault 标记：0 表示不用 vault。
    payload.push(0);
    if let Some(expires_after) = expires_after {
        // 有效期标记：0 表示后面跟一个毫秒时间戳。
        payload.push(0);
        payload.extend_from_slice(&expires_after.to_be_bytes());
    }
    Ok(keccak256(&payload))
}

/// 对动作哈希做 L1 签名。
pub(crate) fn sign_l1_action(
    key: &SecretKey,
    network: Network,
    connection_id: [u8; 32],
) -> Result<Signature, SigningError> {
    let digest = agent_digest(source_of(network), connection_id);
    let signing_key: &SigningKey = key.signing_key();
    let (signature, recovery_id) = signing_key
        .sign_prehash_recoverable(&digest)
        .map_err(|_| SigningError::Sign)?;
    let bytes = signature.to_bytes();
    let mut r = [0_u8; 32];
    let mut s = [0_u8; 32];
    r.copy_from_slice(&bytes[..32]);
    s.copy_from_slice(&bytes[32..]);
    Ok(Signature {
        r,
        s,
        // 以太坊用的是 27 + recovery_id。
        v: recovery_id.to_byte() + 27,
    })
}

/// EIP-712 里 `source` 字段的取值。
fn source_of(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "a",
        Network::Testnet => "b",
    }
}

fn agent_digest(source: &str, connection_id: [u8; 32]) -> [u8; 32] {
    let mut payload = Vec::with_capacity(66);
    payload.extend_from_slice(b"\x19\x01");
    payload.extend_from_slice(&domain_separator());
    payload.extend_from_slice(&agent_message_hash(source, connection_id));
    keccak256(&payload)
}

fn domain_separator() -> [u8; 32] {
    let mut payload = Vec::with_capacity(32 * 5);
    payload.extend_from_slice(&keccak256(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    ));
    payload.extend_from_slice(&keccak256(b"Exchange"));
    payload.extend_from_slice(&keccak256(b"1"));
    let mut chain_id = [0_u8; 32];
    chain_id[24..].copy_from_slice(&DOMAIN_CHAIN_ID.to_be_bytes());
    payload.extend_from_slice(&chain_id);
    payload.extend_from_slice(&[0_u8; 32]);
    keccak256(&payload)
}

fn agent_message_hash(source: &str, connection_id: [u8; 32]) -> [u8; 32] {
    let mut payload = Vec::with_capacity(32 * 3);
    payload.extend_from_slice(&keccak256(b"Agent(string source,bytes32 connectionId)"));
    payload.extend_from_slice(&keccak256(source.as_bytes()));
    payload.extend_from_slice(&connection_id);
    keccak256(&payload)
}

fn keccak256(payload: &[u8]) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    hasher.update(payload);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::{action_hash, agent_digest, keccak256, sign_l1_action, source_of};
    use poise_runtime::{Network, SecretKey};
    use serde::Serialize;

    #[derive(Serialize)]
    struct NoopAction<'a> {
        #[serde(rename = "type")]
        kind: &'a str,
    }

    /// 上游 SDK 的 L1 签名样本（`exchanges/hyperliquid/src/signing.rs` 里用过同一组）。
    #[test]
    fn signs_l1_actions_like_the_published_sdk_sample() {
        let connection_id: [u8; 32] =
            hex::decode("de6c4037798a4434ca03cd05f00e3b803126221375cd1e7eaaaf041768be06eb")
                .unwrap()
                .try_into()
                .unwrap();
        let key =
            SecretKey::parse("0xe908f86dbb4d55ac876378565aafeabc187f6690f046459397b17d9b9a19688e")
                .unwrap();

        assert_eq!(
            sign_l1_action(&key, Network::Mainnet, connection_id)
                .unwrap()
                .to_compact_hex(),
            "fa8a41f6a3fa728206df80801a83bcbfbab08649cd34d9c0bfba7c7b2f99340f53a00226604567b98a1492803190d65a201d6805e5831b7044f17fd530aec7841c"
        );
        assert_eq!(
            sign_l1_action(&key, Network::Testnet, connection_id)
                .unwrap()
                .to_compact_hex(),
            "1713c0fc661b792a50e8ffdd59b637b1ed172d9a3aa4d801d9d88646710fb74b33959f4d075a7ccbec9f2374a6da21ffa4448d58d0413a0d335775f680a881431c"
        );
    }

    /// 签名必须能还原出签名者的地址——交易所就是这么认人的。
    ///
    /// 这条是上面那组固定向量的补充：固定向量只能证明「和上游一致」，
    /// 这一条证明「还原出来的就是私钥对应的地址」。
    #[test]
    fn the_signature_recovers_to_the_address_of_the_signing_key() {
        use k256::ecdsa::{RecoveryId, VerifyingKey};

        let key =
            SecretKey::parse("0x0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        let connection_id = [7_u8; 32];
        let signature = sign_l1_action(&key, Network::Testnet, connection_id).unwrap();

        let mut compact = [0_u8; 64];
        compact[..32].copy_from_slice(&signature.r);
        compact[32..].copy_from_slice(&signature.s);
        let compact = k256::ecdsa::Signature::from_slice(&compact).unwrap();
        let recovery_id = RecoveryId::from_byte(signature.v - 27).unwrap();
        let recovered = VerifyingKey::recover_from_prehash(
            &agent_digest(source_of(Network::Testnet), connection_id),
            &compact,
            recovery_id,
        )
        .unwrap();

        assert_eq!(address_of(&recovered), key.address().to_string());
        // 与 runtime 里那份公开测试向量对齐，防止这里的推导也一起错。
        assert_eq!(
            address_of(&recovered),
            "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf"
        );
    }

    /// 未压缩公钥的 Keccak-256 摘要末 20 字节。
    fn address_of(key: &k256::ecdsa::VerifyingKey) -> String {
        use sha3::{Digest, Keccak256};

        let encoded = key.to_encoded_point(false);
        let digest = Keccak256::digest(&encoded.as_bytes()[1..]);
        format!("0x{}", hex::encode(&digest[12..]))
    }

    #[test]
    fn the_action_hash_covers_the_nonce() {
        let action = NoopAction { kind: "noop" };

        assert_eq!(
            action_hash(&action, 1_700_000_000_000, None).unwrap(),
            action_hash(&action, 1_700_000_000_000, None).unwrap()
        );
        assert_ne!(
            action_hash(&action, 1_700_000_000_000, None).unwrap(),
            action_hash(&action, 1_700_000_000_001, None).unwrap()
        );
    }

    /// 带有效期的动作哈希必须与官方 SDK 的字节布局逐字节一致。
    ///
    /// 期望值这边按 SDK 的 `action_hash` 手工拼一遍：`msgpack(action) ‖ nonce ‖ 0x00
    /// ‖ 0x00 ‖ 有效期`。交易所会独立地重算一次，拼错就只会得到
    /// `User or API Wallet … does not exist`。
    #[test]
    fn the_action_hash_appends_the_expiry_the_way_the_sdk_does() {
        let action = NoopAction { kind: "noop" };
        let nonce = 1_700_000_000_000_u64;
        let expires_after = 1_700_000_005_000_u64;

        let mut expected = rmp_serde::to_vec_named(&action).unwrap();
        expected.extend_from_slice(&nonce.to_be_bytes());
        expected.push(0); // 不用 vault
        expected.push(0); // 后面跟一个有效期
        expected.extend_from_slice(&expires_after.to_be_bytes());

        assert_eq!(
            action_hash(&action, nonce, Some(expires_after)).unwrap(),
            keccak256(&expected)
        );
    }

    /// 有效期要真的参与哈希，而且只在带的时候参与。
    #[test]
    fn the_action_hash_covers_the_expiry_only_when_it_is_there() {
        let action = NoopAction { kind: "noop" };
        let nonce = 1_700_000_000_000_u64;

        let mut without = rmp_serde::to_vec_named(&action).unwrap();
        without.extend_from_slice(&nonce.to_be_bytes());
        without.push(0);

        assert_eq!(
            action_hash(&action, nonce, None).unwrap(),
            keccak256(&without)
        );
        assert_ne!(
            action_hash(&action, nonce, Some(1_700_000_005_000)).unwrap(),
            action_hash(&action, nonce, None).unwrap()
        );
        assert_ne!(
            action_hash(&action, nonce, Some(1_700_000_005_000)).unwrap(),
            action_hash(&action, nonce, Some(1_700_000_005_001)).unwrap()
        );
    }

    #[test]
    fn the_signature_serializes_as_hyperliquid_expects() {
        let key =
            SecretKey::parse("0x0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        let signature = sign_l1_action(&key, Network::Testnet, [7_u8; 32]).unwrap();

        let value = serde_json::to_value(&signature).unwrap();

        assert!(value["r"].as_str().unwrap().starts_with("0x"));
        assert_eq!(value["r"].as_str().unwrap().len(), 66);
        assert_eq!(value["s"].as_str().unwrap().len(), 66);
        assert!(matches!(value["v"].as_u64(), Some(27 | 28)));
    }
}
