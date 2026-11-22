//! Values for secrets that manifests declare instead of setting manually.
use super::SecretError;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use openssl::{pkey::PKey, rsa::Rsa};
use piqueld_core::manifest::{SecretEncoding, SecretGenerator};
use zeroize::Zeroizing;

/// Produces a fresh value for a declared secret. RSA generation is CPU-bound,
/// so async callers run it on a blocking thread.
pub(crate) trait Generate {
    /// Returns a new value: random bytes as lowercase hex or unpadded
    /// `base64url` text, or an RSA private key as PKCS#8 PEM. Fails when the
    /// operating system's random source or OpenSSL key generation fails.
    fn generate(&self) -> Result<Zeroizing<Vec<u8>>, SecretError>;
}

impl Generate for SecretGenerator {
    fn generate(&self) -> Result<Zeroizing<Vec<u8>>, SecretError> {
        match *self {
            Self::Random { bytes, encoding } => {
                let mut raw = Zeroizing::new(vec![0; usize::from(bytes)]);
                getrandom::fill(&mut raw).map_err(SecretError::Random)?;
                Ok(match encoding {
                    SecretEncoding::Hex => {
                        const DIGITS: &[u8; 16] = b"0123456789abcdef";
                        let mut text = Zeroizing::new(Vec::with_capacity(raw.len() * 2));
                        for byte in raw.iter() {
                            text.push(DIGITS[usize::from(byte >> 4)]);
                            text.push(DIGITS[usize::from(byte & 0xf)]);
                        }
                        text
                    }
                    SecretEncoding::Base64url => {
                        Zeroizing::new(URL_SAFE_NO_PAD.encode(&*raw).into_bytes())
                    }
                })
            }
            Self::Rsa { bits } => {
                let rsa = |action| move |source| SecretError::Rsa { action, source };
                let key = Rsa::generate(u32::from(bits)).map_err(rsa("generate RSA key"))?;
                let pem = PKey::from_rsa(key)
                    .and_then(|key| key.private_key_to_pem_pkcs8())
                    .map_err(rsa("encode RSA key as PKCS#8 PEM"))?;
                Ok(Zeroizing::new(pem))
            }
        }
    }
}

#[cfg(test)]
mod tests;
