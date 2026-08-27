//! Two providers, ordered, with metrics wired through an observer.
//!
//! The shape most integrations need but rarely have: selecting one of two
//! provisioned accounts at startup leaves the other as dead config.
//!
//!   cargo run --example failover -- 9995551212

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use mdsmedia::{Config, Event, FailoverClient, MdsClient, Observer, Template};

/// Counts what an operator actually wants alerts on. This logic often ends up
/// buried inside the SMS transport; here it is the caller's business.
#[derive(Default)]
struct Metrics {
    accepted: AtomicUsize,
    retries: AtomicUsize,
    failovers: AtomicUsize,
    failures: AtomicUsize,
    slow: AtomicUsize,
}

impl Observer for Metrics {
    fn on_event(&self, event: Event<'_>) {
        let counter = match event {
            Event::Accepted { .. } => &self.accepted,
            Event::Retrying { .. } => &self.retries,
            Event::FailingOver { .. } => &self.failovers,
            Event::Failed { .. } => &self.failures,
            Event::Slow { .. } => &self.slow,
            _ => return,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let to = std::env::args().nth(1).ok_or("usage: failover <number>")?;

    let metrics = Arc::new(Metrics::default());
    // Arc<Metrics> coerces to Arc<dyn Observer>, so one sink serves both
    // clients and the chain.
    let sink: Arc<dyn Observer> = metrics.clone();
    let base = Config::from_env()?;

    // Primary: the dedicated-IP endpoint, plain HTTP.
    let primary = MdsClient::builder()
        .name("mds1")
        .config(base.clone())
        .shared_observer(Arc::clone(&sink))
        .build()?;

    // Fallback: the canonical HTTPS host. Same credentials in this account,
    // so only the endpoint differs.
    let fallback = MdsClient::builder()
        .name("mds")
        .config(Config {
            base_url: mdsmedia::DEFAULT_BASE_URL.to_string(),
            ..base
        })
        .shared_observer(Arc::clone(&sink))
        .build()?;

    let client = FailoverClient::new(vec![primary, fallback])?.shared_observer(sink);

    println!("chain: {}", client.provider_names().join(" -> "));

    let tpl = Template::new("{#var#} is your verification code. Do not share it with anyone.");
    match client.send_otp(&to, "482913", &tpl).await {
        Ok(r) => println!("accepted by {} in {:?}: {:?}", r.provider, r.elapsed, r.message_id),
        Err(e) => eprintln!("every provider failed: {e}"),
    }

    println!(
        "accepted={} retries={} failovers={} failures={} slow={}",
        metrics.accepted.load(Ordering::Relaxed),
        metrics.retries.load(Ordering::Relaxed),
        metrics.failovers.load(Ordering::Relaxed),
        metrics.failures.load(Ordering::Relaxed),
        metrics.slow.load(Ordering::Relaxed),
    );
    Ok(())
}
