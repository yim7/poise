use anyhow::{Context, Result, anyhow};
use k256::ecdsa::SigningKey;
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use sha3::{Digest, Keccak256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HyperliquidChain {
    Mainnet,
    Testnet,
}

impl HyperliquidChain {
    fn source(self) -> &'static str {
        match self {
            Self::Mainnet => "a",
            Self::Testnet => "b",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Signature {
    r: [u8; 32],
    s: [u8; 32],
    v: u8,
}

impl Signature {
    pub(crate) fn to_compact_hex(&self) -> String {
        let mut bytes = Vec::with_capacity(65);
        bytes.extend_from_slice(&self.r);
        bytes.extend_from_slice(&self.s);
        bytes.push(self.v);
        hex::encode(bytes)
    }
}

impl Serialize for Signature {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("Signature", 3)?;
        state.serialize_field("r", &format!("0x{}", hex::encode(self.r)))?;
        state.serialize_field("s", &format!("0x{}", hex::encode(self.s)))?;
        state.serialize_field("v", &self.v)?;
        state.end()
    }
}

pub(crate) fn action_hash<T: Serialize>(
    action: &T,
    nonce: u64,
    vault_address: Option<&str>,
) -> Result<[u8; 32]> {
    let mut payload =
        rmp_serde::to_vec_named(action).context("failed to msgpack encode Hyperliquid action")?;
    payload.extend_from_slice(&nonce.to_be_bytes());
    match vault_address {
        None => payload.push(0),
        Some(vault_address) => {
            payload.push(1);
            payload.extend_from_slice(&parse_address(vault_address)?);
        }
    }
    Ok(keccak256(&payload))
}

pub(crate) fn sign_l1_action(
    private_key: &str,
    chain: HyperliquidChain,
    connection_id: [u8; 32],
) -> Result<Signature> {
    let signing_key = signing_key_from_hex(private_key)?;
    let digest = l1_agent_digest(chain, connection_id);
    let (signature, recovery_id) = signing_key
        .sign_prehash_recoverable(&digest)
        .context("failed to sign Hyperliquid L1 action")?;
    let signature_bytes = signature.to_bytes();
    let mut r = [0_u8; 32];
    let mut s = [0_u8; 32];
    r.copy_from_slice(&signature_bytes[..32]);
    s.copy_from_slice(&signature_bytes[32..]);

    Ok(Signature {
        r,
        s,
        v: recovery_id.to_byte() + 27,
    })
}

fn l1_agent_digest(chain: HyperliquidChain, connection_id: [u8; 32]) -> [u8; 32] {
    let domain_separator = eip712_domain_separator();
    let message_hash = l1_agent_message_hash(chain.source(), connection_id);
    let mut payload = Vec::with_capacity(66);
    payload.extend_from_slice(b"\x19\x01");
    payload.extend_from_slice(&domain_separator);
    payload.extend_from_slice(&message_hash);
    keccak256(&payload)
}

fn eip712_domain_separator() -> [u8; 32] {
    let mut payload = Vec::with_capacity(32 * 5);
    payload.extend_from_slice(&keccak256(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    ));
    payload.extend_from_slice(&keccak256(b"Exchange"));
    payload.extend_from_slice(&keccak256(b"1"));
    let mut chain_id = [0_u8; 32];
    chain_id[30] = 0x05;
    chain_id[31] = 0x39;
    payload.extend_from_slice(&chain_id);
    payload.extend_from_slice(&[0_u8; 32]);
    keccak256(&payload)
}

fn l1_agent_message_hash(source: &str, connection_id: [u8; 32]) -> [u8; 32] {
    let mut payload = Vec::with_capacity(32 * 3);
    payload.extend_from_slice(&keccak256(b"Agent(string source,bytes32 connectionId)"));
    payload.extend_from_slice(&keccak256(source.as_bytes()));
    payload.extend_from_slice(&connection_id);
    keccak256(&payload)
}

fn signing_key_from_hex(private_key: &str) -> Result<SigningKey> {
    let private_key = private_key.trim().trim_start_matches("0x");
    let bytes = hex::decode(private_key).context("invalid Hyperliquid private key hex")?;
    SigningKey::from_slice(&bytes)
        .map_err(|error| anyhow!("invalid Hyperliquid private key: {error}"))
}

fn parse_address(address: &str) -> Result<[u8; 20]> {
    let address = address.trim().trim_start_matches("0x");
    let bytes = hex::decode(address).context("invalid Hyperliquid address hex")?;
    bytes
        .try_into()
        .map_err(|_| anyhow!("invalid Hyperliquid address length"))
}

fn keccak256(payload: &[u8]) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    hasher.update(payload);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::{HyperliquidChain, action_hash, sign_l1_action};
    use serde::Serialize;

    #[derive(Serialize)]
    struct NoopAction<'a> {
        #[serde(rename = "type")]
        type_: &'a str,
    }

    #[test]
    fn signs_l1_agent_like_hyperliquid_sdk() {
        let connection_id =
            hex::decode("de6c4037798a4434ca03cd05f00e3b803126221375cd1e7eaaaf041768be06eb")
                .unwrap();
        let connection_id: [u8; 32] = connection_id.try_into().unwrap();

        let mainnet_signature = sign_l1_action(
            "e908f86dbb4d55ac876378565aafeabc187f6690f046459397b17d9b9a19688e",
            HyperliquidChain::Mainnet,
            connection_id,
        )
        .unwrap();
        let testnet_signature = sign_l1_action(
            "e908f86dbb4d55ac876378565aafeabc187f6690f046459397b17d9b9a19688e",
            HyperliquidChain::Testnet,
            connection_id,
        )
        .unwrap();

        assert_eq!(
            mainnet_signature.to_compact_hex(),
            "fa8a41f6a3fa728206df80801a83bcbfbab08649cd34d9c0bfba7c7b2f99340f53a00226604567b98a1492803190d65a201d6805e5831b7044f17fd530aec7841c"
        );
        assert_eq!(
            testnet_signature.to_compact_hex(),
            "1713c0fc661b792a50e8ffdd59b637b1ed172d9a3aa4d801d9d88646710fb74b33959f4d075a7ccbec9f2374a6da21ffa4448d58d0413a0d335775f680a881431c"
        );
    }

    #[test]
    fn action_hash_includes_nonce_and_optional_vault_address() {
        let action = NoopAction { type_: "noop" };
        let without_vault = action_hash(&action, 1_700_000_000_000, None).unwrap();
        let without_vault_again = action_hash(&action, 1_700_000_000_000, None).unwrap();
        let different_nonce = action_hash(&action, 1_700_000_000_001, None).unwrap();
        let with_vault = action_hash(
            &action,
            1_700_000_000_000,
            Some("0x3333333333333333333333333333333333333333"),
        )
        .unwrap();

        assert_eq!(without_vault, without_vault_again);
        assert_ne!(without_vault, with_vault);
        assert_ne!(without_vault, different_nonce);
    }
}
