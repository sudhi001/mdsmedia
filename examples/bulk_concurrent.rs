//! Fan a batch out concurrently and summarise the outcome.
//!
//!   cargo run --example bulk_concurrent -- 9995551212 9995551213 9995551214

use std::time::{Duration, Instant};

use mdsmedia::{MdsClient, Message, RetryPolicy};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let numbers: Vec<String> = std::env::args().skip(1).collect();
    if numbers.is_empty() {
        return Err("usage: bulk_concurrent <number>...".into());
    }

    let client = MdsClient::builder()
        .config(mdsmedia::Config::from_env()?)
        .timeout(Duration::from_secs(15))
        .concurrency(32)
        .retry(RetryPolicy {
            max_retries: 2,
            ..RetryPolicy::default()
        })
        .build()?;

    let messages: Vec<Message> = numbers
        .iter()
        .enumerate()
        .map(|(i, n)| {
            Message::new(n, "Scheduled maintenance tonight 11pm-1am.")
                .reference(format!("batch-1/{i}"))
        })
        .collect();

    let started = Instant::now();
    let results = client.send_many(messages).await;
    let elapsed = started.elapsed();

    let mut ok = 0;
    for (to, result) in &results {
        match result {
            Ok(r) => {
                ok += 1;
                println!("OK   {to} id={:?} ref={:?}", r.message_id, r.reference);
            }
            Err(e) => println!("FAIL {to} {e}"),
        }
    }
    println!("{ok}/{} sent in {elapsed:?}", results.len());
    Ok(())
}
