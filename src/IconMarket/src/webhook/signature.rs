//! Webhook signatures (FR-LINE-02, NFR-SEC-01): `X-Line-Signature` is the Base64-encoded
//! HMAC-SHA256 of the raw request body, keyed with the channel secret.

use axum::http::HeaderValue;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use crate::config::Secret;

/// Checks webhook signatures against the channel secret. It holds the keyed hash state rather
/// than the secret, and has no `Debug`, so it can't leak into logs (CFG-06).
#[derive(Clone)]
pub struct Verifier(Hmac<Sha256>);

impl Verifier {
    pub fn new(channel_secret: &Secret) -> Self {
        let mac = Hmac::new_from_slice(channel_secret.expose().as_bytes())
            .expect("HMAC takes a key of any length");
        Self(mac)
    }

    /// Whether `signature`, the `X-Line-Signature` header, signs `body`. The comparison takes
    /// the same time however much of the signature matches.
    pub fn verify(&self, signature: Option<&HeaderValue>, body: &[u8]) -> bool {
        let Some(tag) = signature.and_then(|value| STANDARD.decode(value.as_bytes()).ok()) else {
            return false;
        };
        let mut mac = self.0.clone();
        mac.update(body);
        mac.verify_slice(&tag).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "channel-secret";
    const BODY: &str = r#"{"destination":"U0123456789abcdef0123456789abcdef","events":[]}"#;
    /// `printf '%s' "$BODY" | openssl dgst -sha256 -hmac channel-secret -binary | base64`
    const SIGNATURE: &str = "ezxxOQ39M2U+Dzkj4SllRIn//eHFoa+a4Hn2YMnYK88=";

    fn verify(secret: &str, signature: Option<&str>, body: &str) -> bool {
        let header = signature.map(|s| HeaderValue::from_str(s).unwrap());
        Verifier::new(&Secret::new(secret)).verify(header.as_ref(), body.as_bytes())
    }

    #[test]
    fn a_signature_made_with_the_channel_secret_is_accepted() {
        assert!(verify(SECRET, Some(SIGNATURE), BODY));
    }

    #[test]
    fn anything_else_is_refused() {
        let truncated = &SIGNATURE[..SIGNATURE.len() - 4];
        let other_body = BODY.replace("[]", "[ ]");
        for (secret, signature, body) in [
            (SECRET, None, BODY),
            (SECRET, Some(""), BODY),
            (SECRET, Some("not base64!"), BODY),
            (SECRET, Some(truncated), BODY),
            (SECRET, Some(SIGNATURE), other_body.as_str()),
            ("another-secret", Some(SIGNATURE), BODY),
        ] {
            assert!(
                !verify(secret, signature, body),
                "{secret} {signature:?} {body}"
            );
        }
    }
}
