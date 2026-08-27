//! Ordered failover across several provider accounts.
//!
//! MDS accounts are commonly provisioned in pairs — a canonical HTTPS host and
//! a dedicated IP over plain HTTP. Integrations typically select one at
//! startup and never reconsider:
//!
//! ```text
//! if primary == "b" { use_b() } else { use_a() }
//! ```
//!
//! Two working accounts, one ever used. When the selected endpoint is down,
//! every message fails while a healthy account sits idle. This type is that
//! missing `else`.

use std::sync::Arc;

use futures_util::stream::{FuturesUnordered, StreamExt};

use crate::client::BatchItem;
use crate::error::ProviderFailure;
use crate::observer::{Event, Observer};
use crate::{Error, MdsClient, Message, Response, Result, Template};

/// Sends through a list of providers in priority order, moving to the next
/// only when a failure could plausibly be provider-specific.
///
/// ```no_run
/// # async fn demo() -> mdsmedia::Result<()> {
/// use mdsmedia::{FailoverClient, MdsClient};
///
/// let primary = MdsClient::builder()
///     .name("mds1")
///     .base_url("http://XXX.XXX.XXX.XXX/api.php")
///     .username("XXXXXX").api_key("k").sender_id("SENDER")
///     .build()?;
///
/// let backup = MdsClient::builder()
///     .name("mds")                     // defaults to https://mdssend.in/api.php
///     .username("XXXXXX").api_key("k").sender_id("SENDER")
///     .build()?;
///
/// let client = FailoverClient::new(vec![primary, backup])?;
/// let resp = client.send("PHONE_NUMBER", "hello").await?;
/// println!("served by {}", resp.provider);
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct FailoverClient {
    providers: Arc<Vec<MdsClient>>,
    observer: Option<Arc<dyn Observer>>,
}

impl std::fmt::Debug for FailoverClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FailoverClient")
            .field("providers", &self.provider_names())
            .field("has_observer", &self.observer.is_some())
            .finish()
    }
}

impl FailoverClient {
    /// Builds a failover chain. Providers are tried in the order given.
    ///
    /// Returns [`Error::MissingConfig`] if the list is empty — a chain with no
    /// providers can only ever fail, and failing at construction is better
    /// than at 3am.
    pub fn new(providers: Vec<MdsClient>) -> Result<Self> {
        if providers.is_empty() {
            return Err(Error::MissingConfig("providers"));
        }
        Ok(FailoverClient {
            providers: Arc::new(providers),
            observer: None,
        })
    }

    /// Registers a hook that sees [`Event::FailingOver`]. Per-provider events
    /// still go to each client's own observer.
    pub fn observer<O: Observer>(mut self, observer: O) -> Self {
        self.observer = Some(Arc::new(observer));
        self
    }

    /// Shares an already-constructed observer, so one sink can serve the chain
    /// and every client in it.
    pub fn shared_observer(mut self, observer: Arc<dyn Observer>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Provider names, in priority order.
    pub fn provider_names(&self) -> Vec<&str> {
        self.providers.iter().map(MdsClient::name).collect()
    }

    /// The provider tried first.
    pub fn primary(&self) -> &MdsClient {
        &self.providers[0]
    }

    /// Sends literal text, falling back as needed.
    pub async fn send(&self, to: impl Into<String>, body: impl Into<String>) -> Result<Response> {
        self.send_message(Message::new(to, body)).await
    }

    /// Renders a DLT template with a single variable and sends it.
    ///
    /// Each provider carries its own `TID`, so the template must be registered
    /// against every account in the chain or the fallback will be rejected by
    /// the carrier even though the gateway accepts it.
    pub async fn send_otp(
        &self,
        to: impl Into<String>,
        otp: impl AsRef<str>,
        template: &Template,
    ) -> Result<Response> {
        self.send_message(Message::new(to, template.render_one(otp)))
            .await
    }

    /// Tries each provider in order until one accepts the message.
    pub async fn send_message(&self, message: Message) -> Result<Response> {
        let mut failures: Vec<ProviderFailure> = Vec::new();

        for (idx, provider) in self.providers.iter().enumerate() {
            match provider.send_message(message.clone()).await {
                Ok(response) => return Ok(response),
                Err(err) => {
                    // A bad recipient or an oversized body fails identically
                    // everywhere. Trying the next provider just wastes a
                    // round-trip and muddies the error.
                    if !is_worth_failing_over(&err) {
                        return if failures.is_empty() {
                            Err(err)
                        } else {
                            failures.push(ProviderFailure {
                                provider: provider.name().to_string(),
                                error: err,
                            });
                            Err(Error::AllProvidersFailed(failures))
                        };
                    }

                    if let Some(next) = self.providers.get(idx + 1) {
                        if let Some(observer) = &self.observer {
                            observer.on_event(Event::FailingOver {
                                from: provider.name(),
                                to_provider: next.name(),
                                to: message.recipient(),
                                error: &err,
                            });
                        }
                    }

                    failures.push(ProviderFailure {
                        provider: provider.name().to_string(),
                        error: err,
                    });
                }
            }
        }

        Err(Error::AllProvidersFailed(failures))
    }

    /// Sends a batch concurrently, each message failing over independently.
    ///
    /// Concurrency is taken from the primary provider's setting. Results come
    /// back in input order.
    pub async fn send_many(&self, messages: Vec<Message>) -> Vec<BatchItem> {
        let limit = self.primary().concurrency();
        self.send_many_with_concurrency(messages, limit).await
    }

    /// [`send_many`](Self::send_many) with an explicit concurrency cap.
    pub async fn send_many_with_concurrency(
        &self,
        messages: Vec<Message>,
        concurrency: usize,
    ) -> Vec<BatchItem> {
        let total = messages.len();
        if total == 0 {
            return Vec::new();
        }
        let mut slots: Vec<Option<BatchItem>> = (0..total).map(|_| None).collect();
        let limit = concurrency.max(1).min(total);

        let mut queue = messages.into_iter().enumerate();
        let mut in_flight = FuturesUnordered::new();

        for _ in 0..limit {
            match queue.next() {
                Some((idx, msg)) => in_flight.push(self.send_indexed(idx, msg)),
                None => break,
            }
        }

        while let Some((idx, to, result)) = in_flight.next().await {
            slots[idx] = Some((to, result));
            if let Some((next_idx, next_msg)) = queue.next() {
                in_flight.push(self.send_indexed(next_idx, next_msg));
            }
        }

        slots.into_iter().flatten().collect()
    }

    async fn send_indexed(&self, idx: usize, message: Message) -> (usize, String, Result<Response>) {
        let to = message.recipient().to_string();
        let result = self.send_message(message).await;
        (idx, to, result)
    }
}

/// Whether a failure on one provider might succeed on another.
///
/// Recipient and body problems are deterministic — the number is malformed or
/// the text is too long no matter who carries it. Everything else (transport,
/// 5xx, exhausted retries, and gateway rejections, which are usually
/// credential- or DLT-template-specific) is worth retrying elsewhere.
fn is_worth_failing_over(err: &Error) -> bool {
    !matches!(
        err,
        Error::InvalidNumber { .. } | Error::InvalidMessage(_) | Error::MissingConfig(_)
    )
}
