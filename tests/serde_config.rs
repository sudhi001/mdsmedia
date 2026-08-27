#![cfg(all(feature = "serde", feature = "json"))]

use mdsmedia::{Config, MdsClient, Route, DEFAULT_BASE_URL};

/// Mirrors the shape of a typical YAML provider block, so an operator can move
/// credentials over without re-keying them.
#[test]
fn config_deserializes_from_the_existing_service_shape() {
    let json = r#"{
        "base_url": "http://203.0.113.10/api.php",
        "username": "123456",
        "api_key": "YOUR_API_KEY",
        "sender_id": "SENDER",
        "route": "transactional",
        "template_id": "1234567890123456789",
        "entity_id": "9876543210987654321",
        "default_country_code": "91"
    }"#;

    let config: Config = serde_json::from_str(json).unwrap();
    assert_eq!(config.route, Route::Transactional);
    assert_eq!(config.template_id.as_deref(), Some("1234567890123456789"));
    assert!(MdsClient::from_config(config).is_ok());
}

/// Only the three credential fields are required. Everything else defaults —
/// notably `base_url`, so a config file need only carry the account secrets.
#[test]
fn optional_fields_may_be_omitted() {
    let json = r#"{
        "username": "123456",
        "api_key": "k",
        "sender_id": "SENDER"
    }"#;
    let config: Config = serde_json::from_str(json).unwrap();
    assert_eq!(config.base_url, DEFAULT_BASE_URL);
    assert_eq!(config.route, Route::Transactional);
    assert!(config.template_id.is_none());
    assert!(MdsClient::from_config(config).is_ok());
}
