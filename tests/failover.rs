//! Failover and observability, against mock gateways.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mdsmedia::{Config, Error, Event, FailoverClient, MdsClient, Message, RetryPolicy};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(name: &str, server: &MockServer) -> MdsClient {
    MdsClient::builder()
        .name(name)
        .config(Config {
            base_url: format!("{}/api.php", server.uri()),
            username: "123456".into(),
            api_key: "k".into(),
            sender_id: "SENDER".into(),
            default_country_code: Some("91".into()),
            ..Default::default()
        })
        .retry(RetryPolicy::none())
        .build()
        .unwrap()
}

async fn responding(body: &str, status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn falls_back_to_the_secondary_when_the_primary_is_down() {
    let down = responding("gateway exploded", 503).await;
    let up = responding("Message Submitted successfully<pre>msg-id : ABC12345", 200).await;

    let failover =
        FailoverClient::new(vec![client("mds1", &down), client("mds", &up)]).unwrap();

    let resp = failover.send("9995551212", "hello").await.unwrap();
    // The whole point: the send succeeded, and we know which endpoint served it.
    assert_eq!(resp.provider, "mds");
    assert_eq!(resp.message_id.as_deref(), Some("ABC12345"));
}

#[tokio::test]
async fn the_primary_is_preferred_when_healthy() {
    let up = responding("msg-id : PRIMARY1", 200).await;
    let backup = responding("msg-id : BACKUP01", 200).await;

    let failover =
        FailoverClient::new(vec![client("mds1", &up), client("mds", &backup)]).unwrap();

    let resp = failover.send("9995551212", "hello").await.unwrap();
    assert_eq!(resp.provider, "mds1");
    assert_eq!(resp.message_id.as_deref(), Some("PRIMARY1"));
}

#[tokio::test]
async fn bad_credentials_on_the_primary_still_fail_over() {
    // Each account has its own key, so a rejection on one says nothing
    // about the other.
    let bad_key = responding("ERROR: Invalid API Key", 200).await;
    let good = responding("msg-id : ABC12345", 200).await;

    let failover =
        FailoverClient::new(vec![client("mds1", &bad_key), client("mds", &good)]).unwrap();

    let resp = failover.send("9995551212", "hello").await.unwrap();
    assert_eq!(resp.provider, "mds");
}

#[tokio::test]
async fn a_bad_recipient_does_not_burn_the_fallback() {
    let primary = responding("msg-id : ABC12345", 200).await;
    let backup = responding("msg-id : ABC12345", 200).await;
    // The backup must never be contacted: the number is malformed everywhere.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&backup)
        .await;

    let failover =
        FailoverClient::new(vec![client("mds1", &primary), client("mds", &backup)]).unwrap();

    let err = failover.send("12345", "hello").await.unwrap_err();
    assert!(matches!(err, Error::InvalidNumber { .. }), "got {err:?}");
}

#[tokio::test]
async fn every_provider_failing_reports_all_causes() {
    let a = responding("gateway exploded", 503).await;
    let b = responding("ERROR: Invalid API Key", 200).await;

    let failover = FailoverClient::new(vec![client("mds1", &a), client("mds", &b)]).unwrap();
    let err = failover.send("9995551212", "hello").await.unwrap_err();

    let Error::AllProvidersFailed(failures) = &err else {
        panic!("expected AllProvidersFailed, got {err:?}");
    };
    assert_eq!(failures.len(), 2);
    assert_eq!(failures[0].provider, "mds1");
    assert_eq!(failures[1].provider, "mds");
    // The message names both, so the cause is not flattened to the last one.
    let text = err.to_string();
    assert!(text.contains("mds1"), "{text}");
    assert!(text.contains("Invalid API Key"), "{text}");
    // One transient failure means the batch is worth re-queuing later.
    assert!(err.is_retryable());
}

#[tokio::test]
async fn an_empty_chain_is_rejected_at_construction() {
    assert!(matches!(
        FailoverClient::new(vec![]).unwrap_err(),
        Error::MissingConfig("providers")
    ));
}

#[tokio::test]
async fn batches_fail_over_per_message() {
    let down = responding("gateway exploded", 503).await;
    let up = responding("msg-id : ABC12345", 200).await;
    let failover =
        FailoverClient::new(vec![client("mds1", &down), client("mds", &up)]).unwrap();

    let messages: Vec<Message> = (0..6)
        .map(|i| Message::new(format!("99955512{i:02}"), "hi"))
        .collect();
    let results = failover.send_many_with_concurrency(messages, 3).await;

    assert_eq!(results.len(), 6);
    assert!(results.iter().all(|(_, r)| r.is_ok()));
    assert!(results
        .iter()
        .all(|(_, r)| r.as_ref().unwrap().provider == "mds"));
}

#[tokio::test]
async fn observers_see_the_send_lifecycle() {
    let server = responding("msg-id : ABC12345", 200).await;
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);

    let client = MdsClient::builder()
        .name("mds1")
        .config(Config {
            base_url: format!("{}/api.php", server.uri()),
            username: "u".into(),
            api_key: "k".into(),
            sender_id: "S".into(),
            ..Default::default()
        })
        .observer(move |event: Event<'_>| {
            let label = match event {
                Event::Attempt { .. } => "attempt",
                Event::Accepted { .. } => "accepted",
                Event::Retrying { .. } => "retrying",
                Event::Failed { .. } => "failed",
                Event::Slow { .. } => "slow",
                Event::FailingOver { .. } => "failing_over",
                _ => "other",
            };
            sink.lock().unwrap().push(format!("{}:{label}", event.provider()));
        })
        .build()
        .unwrap();

    client.send("9995551212", "hi").await.unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["mds1:attempt", "mds1:accepted"]
    );
}

#[tokio::test]
async fn observers_see_retries_and_the_final_failure() {
    let server = responding("gateway exploded", 503).await;
    let retries = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(0));
    let (r, f) = (Arc::clone(&retries), Arc::clone(&failures));

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
            max_backoff: Duration::from_millis(2),
            multiplier_pct: 200,
            jitter_pct: 0,
        })
        .observer(move |event: Event<'_>| match event {
            Event::Retrying { .. } => {
                r.fetch_add(1, Ordering::Relaxed);
            }
            Event::Failed { .. } => {
                f.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        })
        .build()
        .unwrap();

    assert!(client.send("9995551212", "hi").await.is_err());
    assert_eq!(retries.load(Ordering::Relaxed), 2);
    // Exactly one terminal failure, not one per attempt. Emitting per-attempt
    // is what drives integrations to bolt a throttle onto their alerting.
    assert_eq!(failures.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn slow_sends_are_reported() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("msg-id : ABC12345")
                .set_delay(Duration::from_millis(120)),
        )
        .mount(&server)
        .await;

    let slow = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&slow);
    let client = MdsClient::builder()
        .config(Config {
            base_url: format!("{}/api.php", server.uri()),
            username: "u".into(),
            api_key: "k".into(),
            sender_id: "S".into(),
            ..Default::default()
        })
        .slow_after(Duration::from_millis(50))
        .observer(move |event: Event<'_>| {
            if matches!(event, Event::Slow { .. }) {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        })
        .build()
        .unwrap();

    client.send("9995551212", "hi").await.unwrap();
    assert_eq!(slow.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn failover_notifies_before_switching() {
    let down = responding("gateway exploded", 503).await;
    let up = responding("msg-id : ABC12345", 200).await;

    let hops: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&hops);

    let failover = FailoverClient::new(vec![client("mds1", &down), client("mds", &up)])
        .unwrap()
        .observer(move |event: Event<'_>| {
            if let Event::FailingOver { from, to_provider, .. } = event {
                sink.lock().unwrap().push(format!("{from}->{to_provider}"));
            }
        });

    failover.send("9995551212", "hi").await.unwrap();
    assert_eq!(*hops.lock().unwrap(), vec!["mds1->mds"]);
}

/// The API key travels in the query string, so any error that carries a URL
/// leaks it into logs. Every error path must be clean.
#[tokio::test]
async fn errors_never_leak_the_api_key() {
    const SECRET: &str = "super-secret-api-key";

    // 1. Dead endpoint -> transport error.
    let dead = MdsClient::builder()
        .name("dead")
        .config(Config {
            base_url: "http://127.0.0.1:9/api.php".into(),
            username: "123456".into(),
            api_key: SECRET.into(),
            sender_id: "SENDER".into(),
            ..Default::default()
        })
        .retry(RetryPolicy::none())
        .build()
        .unwrap();

    let err = dead.send("9995551212", "hi").await.unwrap_err();
    let rendered = format!("{err} {err:?} {}", source_chain(&err));
    assert!(!rendered.contains(SECRET), "leaked key: {rendered}");
    // The host is still identifiable for debugging.
    assert!(rendered.contains("127.0.0.1:9"), "{rendered}");

    // 2. Aggregate failover error must not leak it either.
    let backup = MdsClient::builder()
        .name("also-dead")
        .config(Config {
            base_url: "http://127.0.0.1:9/api.php".into(),
            username: "123456".into(),
            api_key: SECRET.into(),
            sender_id: "SENDER".into(),
            ..Default::default()
        })
        .retry(RetryPolicy::none())
        .build()
        .unwrap();

    let chain = FailoverClient::new(vec![dead, backup]).unwrap();
    let err = chain.send("9995551212", "hi").await.unwrap_err();
    let rendered = format!("{err} {err:?}");
    assert!(!rendered.contains(SECRET), "leaked key: {rendered}");
}

fn source_chain(err: &dyn std::error::Error) -> String {
    let mut out = String::new();
    let mut cursor = err.source();
    while let Some(e) = cursor {
        out.push_str(&format!(" | {e}"));
        cursor = e.source();
    }
    out
}
