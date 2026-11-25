use super::*;

#[test]
fn random_values_are_fresh_and_encoded() {
    let hex = SecretGenerator::Random {
        bytes: 32,
        encoding: SecretEncoding::Hex,
    };
    let first = hex.generate().expect("the secret generates");
    assert_eq!(first.len(), 64);
    assert!(first.iter().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')));
    assert_ne!(first, hex.generate().expect("the secret generates"));

    let base64 = SecretGenerator::Random {
        bytes: 32,
        encoding: SecretEncoding::Base64url,
    };
    let value = base64.generate().expect("the secret generates");
    assert_eq!(
        URL_SAFE_NO_PAD
            .decode(&*value)
            .expect("the value decodes")
            .len(),
        32
    );
}

#[test]
fn rsa_keys_are_pkcs8_pem() {
    let pem = SecretGenerator::Rsa { bits: 2048 }
        .generate()
        .expect("the secret generates");
    assert!(pem.starts_with(b"-----BEGIN PRIVATE KEY-----"));
    let key = PKey::private_key_from_pem(&pem).expect("the pem parses");
    assert_eq!(key.rsa().expect("the key is rsa").size() * 8, 2048);
}
