//! BTCPay webhook handling.
//!
//! BTCPay signs each webhook body with HMAC-SHA256 using the shared secret
//! we configured, and sends the hex digest in the `BTCPay-Sig` header as
//! `sha256=<hex>`. We verify in constant time before trusting anything.

use anyhow::{anyhow, Result};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Verify the `BTCPay-Sig` header matches the raw request body.
///
/// Returns `Ok(())` on success, `Err` on any mismatch. Callers must pass the
/// raw, unmodified body — any reserialization will break the HMAC.
pub fn verify_signature(secret: &str, header_value: &str, raw_body: &[u8]) -> Result<()> {
    let expected_hex = header_value
        .strip_prefix("sha256=")
        .ok_or_else(|| anyhow!("BTCPay-Sig header missing 'sha256=' prefix"))?;
    let expected =
        hex::decode(expected_hex).map_err(|_| anyhow!("BTCPay-Sig header is not hex"))?;

    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC takes any key size");
    mac.update(raw_body);
    let computed = mac.finalize().into_bytes();

    if bool::from(computed.as_slice().ct_eq(&expected)) {
        Ok(())
    } else {
        Err(anyhow!("BTCPay webhook signature mismatch"))
    }
}

/// The subset of webhook payload fields we care about. BTCPay sends many
/// event types; we key off `invoiceId` and `type` / `status`.
#[derive(Debug, serde::Deserialize)]
pub struct WebhookEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(rename = "invoiceId")]
    pub invoice_id: String,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

impl WebhookEvent {
    /// BTCPay fires event types like `InvoiceSettled`, `InvoiceExpired`,
    /// `InvoiceInvalid`, `InvoiceProcessing`. We normalize to our internal
    /// status vocabulary.
    pub fn to_status(&self) -> Option<&'static str> {
        match self.event_type.as_str() {
            "InvoiceSettled" | "InvoicePaymentSettled" => Some("settled"),
            "InvoiceExpired" => Some("expired"),
            "InvoiceInvalid" => Some("invalid"),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_correct_signature() {
        let secret = "super-secret";
        let body = br#"{"type":"InvoiceSettled","invoiceId":"abc"}"#;

        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let sig = hex::encode(mac.finalize().into_bytes());
        let header = format!("sha256={sig}");

        assert!(verify_signature(secret, &header, body).is_ok());
    }

    #[test]
    fn rejects_tampered_body() {
        let secret = "super-secret";
        let body = br#"{"type":"InvoiceSettled","invoiceId":"abc"}"#;
        let tampered = br#"{"type":"InvoiceSettled","invoiceId":"evil"}"#;

        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let sig = hex::encode(mac.finalize().into_bytes());
        let header = format!("sha256={sig}");

        assert!(verify_signature(secret, &header, tampered).is_err());
    }
}
