//! End-to-end tests against a mock gateway.

use std::time::Duration;

use mdsmedia::{Config, Error, MdsClient, Message, RetryPolicy, Route, Template};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client_for(server: &MockServer) -> MdsClient {
    MdsClient::builder()
        .config(Config {
            base_url: format!("{}/api.php", server.uri()),
            username: "123456".into(),
            api_key: "test-key".into(),
            sender_id: "SENDER".into(),
            route: Route::Transactional,
            template_id: Some("1234567890123456789".into()),
            entity_id: Some("9876543210987654321".into()),
            default_country_code: Some("91".into()),
        })
        .timeout(Duration::from_secs(5))
        .retry(RetryPolicy::none())
        .build()
        .unwrap()
}

#[tokio::test]
async fn sends_every_credential_and_dlt_parameter() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api.php"))
        .and(query_param("username", "123456"))
        .and(query_param("apikey", "test-key"))
        .and(query_param("senderid", "SENDER"))
        .and(query_param("route", "TRANS"))
        .and(query_param("mobile", "919995551212"))
        .and(query_param("TID", "1234567890123456789"))
        .and(query_param("PEID", "9876543210987654321"))
        .and(query_param("text", "482913 is your OTP."))
        .respond_with(ResponseTemplate::new(200).set_body_string("8812345|919995551212"))
        .expect(1)
        .mount(&server)
        .await;

    let tpl = Template::new("{#var#} is your OTP.");
    let resp = client_for(&server)
        .send_otp("9995551212", "482913", &tpl)
        .await
        .unwrap();

    assert_eq!(resp.message_id.as_deref(), Some("8812345"));
    assert_eq!(resp.to, "919995551212");
    assert_eq!(resp.attempts, 1);
}

#[tokio::test]
async fn message_bodies_with_query_metacharacters_survive_intact() {
    let server = MockServer::start().await;
    // A hand-rolled `format!` URL loses everything after the first `&`.
    let tricky = "Pay ₹500 & get 10% off? yes #now";
    Mock::given(method("GET"))
        .and(query_param("text", tricky))
        .respond_with(ResponseTemplate::new(200).set_body_string("8812345"))
        .expect(1)
        .mount(&server)
        .await;

    let resp = client_for(&server)
        .send("9995551212", tricky)
        .await
        .unwrap();
    assert_eq!(resp.message_id.as_deref(), Some("8812345"));
}

#[tokio::test]
async fn http_200_with_an_error_body_is_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ERROR: Invalid API Key"))
        .mount(&server)
        .await;

    let err = client_for(&server)
        .send("9995551212", "hi")
        .await
        .unwrap_err();

    assert!(matches!(err, Error::Rejected { .. }), "got {err:?}");
    assert!(err.is_fatal());
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn rejections_are_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Insufficient Credits"))
        .expect(1) // exactly one attempt, despite retries being enabled
        .mount(&server)
        .await;

    let client = MdsClient::builder()
        .config(Config {
            base_url: format!("{}/api.php", server.uri()),
            username: "u".into(),
            api_key: "k".into(),
            sender_id: "S".into(),
            ..Default::default()
        })
        .retry(RetryPolicy {
            max_retries: 3,
            initial_backoff: Duration::from_millis(1),
            ..RetryPolicy::default()
        })
        .build()
        .unwrap();

    assert!(client.send("9995551212", "hi").await.is_err());
}

#[tokio::test]
async fn server_errors_are_retried_then_surfaced() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
        .expect(3) // 1 initial + 2 retries
        .mount(&server)
        .await;

    let client = MdsClient::builder()
        .config(Config {
            base_url: format!("{}/api.php", server.uri()),
            username: "u".into(),
            api_key: "k".into(),
            sender_id: "S".into(),
            ..Default::default()
        })
        .retry(RetryPolicy {
            max_retries: 2,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(5),
            multiplier_pct: 200,
            jitter_pct: 0,
        })
        .build()
        .unwrap();

    let err = client.send("9995551212", "hi").await.unwrap_err();
    match err {
        Error::RetriesExhausted { attempts, .. } => assert_eq!(attempts, 3),
        other => panic!("expected RetriesExhausted, got {other:?}"),
    }
}

#[tokio::test]
async fn batches_preserve_input_order_and_isolate_failures() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("8812345"))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let messages = vec![
        Message::new("9995551212", "a").reference("first"),
        Message::new("bad-number", "b").reference("second"),
        Message::new("9995551214", "c").reference("third"),
    ];

    let results = client.send_many(messages).await;
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].0, "9995551212");
    assert_eq!(results[1].0, "bad-number");
    assert_eq!(results[2].0, "9995551214");

    assert!(results[0].1.is_ok());
    // The invalid recipient fails locally without sinking its neighbours.
    assert!(matches!(
        results[1].1,
        Err(Error::InvalidNumber { .. })
    ));
    assert!(results[2].1.is_ok());
    assert_eq!(
        results[0].1.as_ref().unwrap().reference.as_deref(),
        Some("first")
    );
}

#[tokio::test]
async fn concurrency_limit_is_respected_and_all_messages_are_sent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("8812345")
                .set_delay(Duration::from_millis(50)),
        )
        .expect(12)
        .mount(&server)
        .await;

    let client = client_for(&server);
    let messages: Vec<Message> = (0..12)
        .map(|i| Message::new(format!("99955512{i:02}"), "hi"))
        .collect();

    let started = std::time::Instant::now();
    let results = client.send_many_with_concurrency(messages, 4).await;
    let elapsed = started.elapsed();

    assert_eq!(results.len(), 12);
    assert!(results.iter().all(|(_, r)| r.is_ok()));
    // 12 messages / 4 in flight x 50ms => ~150ms; serial would be ~600ms.
    assert!(
        elapsed >= Duration::from_millis(140) && elapsed < Duration::from_millis(500),
        "unexpected elapsed {elapsed:?} for concurrency 4"
    );
}

#[tokio::test]
async fn per_message_overrides_win_over_account_defaults() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param("senderid", "OTHER"))
        .and(query_param("route", "PROMO"))
        .and(query_param("TID", "9999"))
        .respond_with(ResponseTemplate::new(200).set_body_string("8812345"))
        .expect(1)
        .mount(&server)
        .await;

    let msg = Message::new("9995551212", "hi")
        .sender_id("OTHER")
        .route(Route::Promotional)
        .template_id("9999");

    assert!(client_for(&server).send_message(msg).await.is_ok());
}

#[tokio::test]
async fn empty_batch_is_a_no_op() {
    let server = MockServer::start().await;
    assert!(client_for(&server).send_many(vec![]).await.is_empty());
}
