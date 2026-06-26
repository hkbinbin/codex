use super::build_pinned_client_config;
use super::build_server_config;
use super::generate_self_signed_tls;
use super::parse_fingerprint_hex;
use pretty_assertions::assert_eq;

#[test]
fn generated_certificate_has_stable_fingerprint_encoding() {
    let tls = generate_self_signed_tls().expect("certificate generation should succeed");
    let hex = tls.fingerprint_hex();
    assert_eq!(hex.len(), 64, "fingerprint hex must be 64 chars");
    let parsed = parse_fingerprint_hex(&hex).expect("round-trip parse should succeed");
    assert_eq!(parsed, tls.fingerprint_sha256);
}

#[test]
fn server_config_builds_from_generated_certificate() {
    let tls = generate_self_signed_tls().expect("certificate generation should succeed");
    let config = build_server_config(tls.cert_der.clone(), tls.key_der.clone_key())
        .expect("server config should build");
    assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
}

#[test]
fn pinned_client_config_advertises_http1_alpn() {
    let tls = generate_self_signed_tls().expect("certificate generation should succeed");
    let config = build_pinned_client_config(tls.fingerprint_sha256);
    assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
}

#[test]
fn parse_fingerprint_accepts_colons_and_mixed_case() {
    let canonical = "a".repeat(64);
    let with_colons = canonical
        .as_bytes()
        .chunks(2)
        .map(|pair| std::str::from_utf8(pair).unwrap())
        .collect::<Vec<_>>()
        .join(":");
    let a = parse_fingerprint_hex(&canonical).expect("plain hex should parse");
    let b = parse_fingerprint_hex(&with_colons).expect("colon-separated hex should parse");
    let c = parse_fingerprint_hex(&canonical.to_uppercase()).expect("uppercase hex should parse");
    assert_eq!(a, b);
    assert_eq!(a, c);
}

#[test]
fn parse_fingerprint_rejects_wrong_length() {
    assert!(parse_fingerprint_hex("abcd").is_err());
    assert!(parse_fingerprint_hex(&"a".repeat(63)).is_err());
    assert!(parse_fingerprint_hex(&"g".repeat(64)).is_err());
}
