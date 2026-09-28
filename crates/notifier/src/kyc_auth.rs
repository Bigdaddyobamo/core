//! Constant-time HMAC-SHA256 verification for inbound KYC webhook authentication.
//! Prevents side-channel timing attacks by ensuring comparison time does not depend
//! on the position of the first mismatched byte.

use anyhow::{anyhow, Result};
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Compares two byte slices in constant time.
/// Returns true if and only if both slices have identical length and contents.
#[inline(never)]
pub fn constant_time_compare(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }
    result == 0
}

/// Verifies an inbound KYC webhook payload using constant-time HMAC-SHA256 comparison.
pub fn verify_kyc_webhook(
    payload_body: &[u8],
    secret_key: &str,
    received_signature_hex: &str,
) -> Result<bool> {
    let clean_hex = received_signature_hex
        .strip_prefix("sha256=")
        .unwrap_or(received_signature_hex)
        .trim();

    let expected_signature_bytes = hex::decode(clean_hex)
        .map_err(|e| anyhow!("invalid hex signature: {}", e))?;

    let mut mac = Hmac::<Sha256>::new_from_slice(secret_key.as_bytes())
        .map_err(|e| anyhow!("invalid HMAC key: {}", e))?;
    mac.update(payload_body);
    let computed_signature = mac.finalize().into_bytes();

    Ok(constant_time_compare(&computed_signature, &expected_signature_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constant_time_compare_identical() {
        let a = b"0123456789abcdef";
        let b = b"0123456789abcdef";
        assert!(constant_time_compare(a, b));
    }

    #[test]
    fn test_constant_time_compare_different_length() {
        let a = b"0123456789abcdef";
        let b = b"0123456789abcde";
        assert!(!constant_time_compare(a, b));
    }

    #[test]
    fn test_constant_time_compare_mismatch() {
        let a = b"0123456789abcdef";
        let b = b"0123456789abcdeg";
        assert!(!constant_time_compare(a, b));
    }

    #[test]
    fn test_verify_kyc_webhook_valid_and_invalid() {
        let secret = "kyc-webhook-secret-token";
        let payload = br#"{"user_id":"usr_123","status":"VERIFIED"}"#;

        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(payload);
        let valid_hex = hex::encode(mac.finalize().into_bytes());

        // Valid signature passes
        assert!(verify_kyc_webhook(payload, secret, &valid_hex).unwrap());
        assert!(verify_kyc_webhook(payload, secret, &format!("sha256={}", valid_hex)).unwrap());

        // Tampered signature fails
        let tampered_hex = format!("00{}", &valid_hex[2..]);
        assert!(!verify_kyc_webhook(payload, secret, &tampered_hex).unwrap());

        // Tampered payload fails
        assert!(!verify_kyc_webhook(b"tampered", secret, &valid_hex).unwrap());
    }
}
