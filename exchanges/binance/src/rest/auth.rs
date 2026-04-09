use hmac::{Hmac, Mac};
use sha2::Sha256;
use url::form_urlencoded::Serializer;

pub(super) fn sign_query(api_secret: &str, query: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(api_secret.as_bytes())
        .expect("HMAC-SHA256 accepts any key length");
    mac.update(query.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

pub(super) fn encode_query(params: &[(&str, String)]) -> String {
    let mut serializer = Serializer::new(String::new());
    for (key, value) in params {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}
