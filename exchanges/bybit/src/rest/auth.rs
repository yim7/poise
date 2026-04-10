use hmac::{Hmac, Mac};
use sha2::Sha256;

pub(crate) fn build_v5_signing_payload(
    timestamp_ms: i64,
    api_key: &str,
    recv_window_ms: i64,
    payload: &str,
) -> String {
    format!("{timestamp_ms}{api_key}{recv_window_ms}{payload}")
}

pub(crate) fn sign_v5_payload(
    api_secret: &str,
    timestamp_ms: i64,
    api_key: &str,
    recv_window_ms: i64,
    payload: &str,
) -> String {
    let signing_payload = build_v5_signing_payload(timestamp_ms, api_key, recv_window_ms, payload);
    let mut mac = Hmac::<Sha256>::new_from_slice(api_secret.as_bytes())
        .expect("HMAC-SHA256 accepts any key length");
    mac.update(signing_payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_v5_payload_with_timestamp_recv_window_and_body() {
        let signature = sign_v5_payload(
            "secret-key",
            1_700_000_000_000,
            "api-key",
            5_000,
            r#"{"symbol":"BTCUSDT"}"#,
        );

        assert_eq!(
            signature,
            "c12472cfb89cef80a14dcb760f2e33587a62b444a4dcfb6d243342752d34051d"
        );
    }
}
