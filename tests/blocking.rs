#![cfg(feature = "blocking")]

use std::time::Duration;

use mdsmedia::blocking::BlockingMdsClient;
use mdsmedia::{Config, Message, Route};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn config(uri: &str) -> Config {
    Config {
        base_url: format!("{uri}/api.php"),
        username: "123456".into(),
        api_key: "test-key".into(),
        sender_id: "SENDER".into(),
        route: Route::Transactional,
        template_id: None,
        entity_id: None,
        default_country_code: Some("91".into()),
    }
}

#[test]
fn blocking_client_sends_and_batches() {
    // The mock server needs its own runtime; the blocking client owns another.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let server = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("8812345")
                    .set_delay(Duration::from_millis(40)),
            )
            .mount(&server)
            .await;
        server
    });

    let client = BlockingMdsClient::from_config(config(&server.uri())).unwrap();

    let single = client.send("9995551212", "hello").unwrap();
    assert_eq!(single.message_id.as_deref(), Some("8812345"));
    assert_eq!(single.to, "919995551212");

    // Batches still run concurrently inside the wrapper's runtime.
    let messages: Vec<Message> = (0..8)
        .map(|i| Message::new(format!("99955512{i:02}"), "hi"))
        .collect();
    let started = std::time::Instant::now();
    let results = client.send_many(messages);
    assert_eq!(results.len(), 8);
    assert!(results.iter().all(|(_, r)| r.is_ok()));
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "blocking send_many appears to be serial"
    );
}
