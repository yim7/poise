use base64::{Engine as _, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub(crate) fn sign_okx_payload(
    timestamp: &str,
    method: &str,
    request_path: &str,
    body: &str,
    secret_key: &str,
) -> String {
    let payload = format!("{timestamp}{method}{request_path}{body}");
    let mut mac = Hmac::<Sha256>::new_from_slice(secret_key.as_bytes())
        .expect("HMAC-SHA256 accepts any key length");
    mac.update(payload.as_bytes());
    STANDARD.encode(mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::sign_okx_payload;

    #[test]
    fn signs_okx_rest_payload_with_hmac_sha256_base64() {
        let signature = sign_okx_payload(
            "2020-12-08T09:08:57.715Z",
            "GET",
            "/api/v5/account/balance?ccy=BTC",
            "",
            "22582BD0CFF14C41EDBF1AB98506286D",
        );

        assert_eq!(signature, "HiZhvSfMtWJA3uUIVXV3a/bSXNPCWvYFXoGCVS8V4zY=");
    }

    #[test]
    fn builds_websocket_login_signature_path() {
        let signature = sign_okx_payload(
            "1704876947",
            "GET",
            "/users/self/verify",
            "",
            "22582BD0CFF14C41EDBF1AB98506286D",
        );

        assert_eq!(signature, "5/36BgGV6m/6pmdc20zdqk0mzF5ZalmzzPD2fo3wavU=");
    }
}
