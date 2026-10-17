use ed25519_dalek::{Signer, SigningKey};

use super::contracts::{
    EVIDENCE_VERSION, EvidenceEnvelope, EvidencePayload, SignatureAlgorithm, signature_message,
};
use crate::policy::issuers::issuer_key_id;
use crate::{Error, Result};

/// Signs a validated canonical payload with an explicitly supplied controller key.
/// This function performs no key discovery and grants no authority to the key.
pub fn sign_evidence(
    payload: EvidencePayload,
    signing_key: &SigningKey,
) -> Result<EvidenceEnvelope> {
    payload.validate()?;
    let public_key_hex = encode_hex(signing_key.verifying_key().as_bytes());
    let expected_key_id = issuer_key_id(&public_key_hex)?;
    if payload.key_id != expected_key_id {
        return Err(Error::Invalid(
            "evidence key ID does not match the supplied signing key".into(),
        ));
    }
    let signature = signing_key.sign(&signature_message(&payload)?);
    Ok(EvidenceEnvelope {
        version: EVIDENCE_VERSION,
        algorithm: SignatureAlgorithm::Ed25519,
        payload,
        signature_hex: encode_hex(&signature.to_bytes()),
    })
}

pub(crate) fn decode_hex<const N: usize>(kind: &str, value: &str) -> Result<[u8; N]> {
    super::contracts::validate_lower_hex(kind, value, N)?;
    let mut output = [0u8; N];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        output[index] = (nibble(pair[0]) << 4) | nibble(pair[1]);
    }
    Ok(output)
}

fn nibble(value: u8) -> u8 {
    match value {
        b'0'..=b'9' => value - b'0',
        b'a'..=b'f' => value - b'a' + 10,
        _ => unreachable!("hex was validated before decoding"),
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0xf) as usize] as char);
    }
    output
}
