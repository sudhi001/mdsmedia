//! Observability hooks.
//!
//! The library deliberately does not log, alert, or email — it reports what
//! happened and lets the caller decide. It is common for this policy to end up
//! *inside* the transport — an error-email call, a throttle to stop outages
//! flooding the inbox, a suppressed-failure counter — which makes the send path
//! impossible to reuse without inheriting the alerting. An [`Observer`] is that
//! seam.

use std::time::Duration;

use crate::{Error, Response};

/// Something that happened during a send.
///
/// Events borrow rather than allocate, so an observer that ignores most
/// variants costs almost nothing.
#[derive(Debug)]
#[non_exhaustive]
pub enum Event<'a> {
    /// A request is about to be issued. `attempt` is 1-based.
    Attempt {
        provider: &'a str,
        to: &'a str,
        attempt: u32,
    },
    /// A transient failure will be retried after `delay`.
    Retrying {
        provider: &'a str,
        to: &'a str,
        attempt: u32,
        delay: Duration,
        error: &'a Error,
    },
    /// The gateway accepted the message.
    Accepted {
        provider: &'a str,
        response: &'a Response,
    },
    /// The send failed and will not be retried on this provider.
    Failed {
        provider: &'a str,
        to: &'a str,
        error: &'a Error,
    },
    /// A send succeeded but took longer than the configured threshold.
    /// Lets callers warn on degraded delivery without the transport
    /// deciding what "too slow" means.
    Slow {
        provider: &'a str,
        to: &'a str,
        elapsed: Duration,
    },
    /// A [`crate::FailoverClient`] is moving to the next provider.
    FailingOver {
        from: &'a str,
        to_provider: &'a str,
        to: &'a str,
        error: &'a Error,
    },
}

impl Event<'_> {
    /// The provider the event originated from.
    pub fn provider(&self) -> &str {
        match self {
            Event::Attempt { provider, .. }
            | Event::Retrying { provider, .. }
            | Event::Accepted { provider, .. }
            | Event::Failed { provider, .. }
            | Event::Slow { provider, .. } => provider,
            Event::FailingOver { from, .. } => from,
        }
    }

    /// Whether this event represents something an operator should see.
    pub fn is_problem(&self) -> bool {
        matches!(
            self,
            Event::Retrying { .. }
                | Event::Failed { .. }
                | Event::Slow { .. }
                | Event::FailingOver { .. }
        )
    }
}

/// Receives [`Event`]s as they occur.
///
/// Called inline on the send path, so implementations must not block. Push to
/// a channel or bump a counter; do the slow work elsewhere.
///
/// Any `Fn(Event<'_>)` is an `Observer`:
///
/// ```
/// use mdsmedia::Event;
/// use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
///
/// let failures = Arc::new(AtomicUsize::new(0));
/// let counter = Arc::clone(&failures);
/// let observer = move |event: Event<'_>| {
///     if matches!(event, Event::Failed { .. }) {
///         counter.fetch_add(1, Ordering::Relaxed);
///     }
/// };
/// # let _ = observer;
/// ```
pub trait Observer: Send + Sync + 'static {
    fn on_event(&self, event: Event<'_>);
}

impl<F> Observer for F
where
    F: Fn(Event<'_>) + Send + Sync + 'static,
{
    fn on_event(&self, event: Event<'_>) {
        self(event)
    }
}
