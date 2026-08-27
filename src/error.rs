//! Error types for the MDS Media client.

use std::time::Duration;

/// Errors returned by [`crate::MdsClient`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A required credential or setting was missing when building the client.
    #[error("missing configuration field: {0}")]
    MissingConfig(&'static str),

    /// `base_url` could not be parsed as a URL.
    #[error("invalid base_url {url:?}: {source}")]
    InvalidBaseUrl {
        url: String,
        #[source]
        source: url::ParseError,
    },

    /// The recipient number failed local validation, before any network call.
    #[error("invalid phone number {number:?}: {reason}")]
    InvalidNumber { number: String, reason: &'static str },

    /// The message body was empty or exceeded the configured maximum length.
    #[error("invalid message: {0}")]
    InvalidMessage(&'static str),

    /// Building the underlying HTTP client failed.
    #[error("failed to build HTTP client: {0}")]
    HttpClientBuild(#[source] reqwest::Error),

    /// Transport-level failure: DNS, connect, TLS, or timeout.
    ///
    /// The URL is deliberately stripped from `source` — it carries the API key
    /// in its query string. `endpoint` names the host instead.
    #[error("transport error contacting {endpoint} after {elapsed:?}: {source}")]
    Transport {
        endpoint: String,
        elapsed: Duration,
        #[source]
        source: reqwest::Error,
    },

    /// The gateway answered, but with a non-2xx HTTP status.
    #[error("gateway returned HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },

    /// The gateway answered 2xx but the payload signalled a failure
    /// (e.g. `ERROR: Invalid API Key`).
    #[error("gateway rejected the message: {reason} (raw: {raw})")]
    Rejected { reason: String, raw: String },

    /// The blocking wrapper could not start its Tokio runtime.
    #[cfg(feature = "blocking")]
    #[error("failed to start the blocking runtime: {0}")]
    Runtime(#[source] std::io::Error),

    /// Every provider in a [`crate::FailoverClient`] failed. Carries each
    /// provider's own error so the cause is not flattened to the last one.
    #[error("all {} provider(s) failed: {}", .0.len(), summarize(.0))]
    AllProvidersFailed(Vec<ProviderFailure>),

    /// Every retry attempt was exhausted. Carries the final underlying error.
    #[error("giving up after {attempts} attempt(s): {source}")]
    RetriesExhausted {
        attempts: u32,
        #[source]
        source: Box<Error>,
    },
}

impl Error {
    /// Whether retrying this error has a realistic chance of succeeding.
    ///
    /// Transport failures and 5xx / 429 responses are retryable. Validation
    /// errors and gateway rejections (bad credentials, bad template) are not —
    /// retrying those just burns quota.
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Transport { source, .. } => {
                source.is_timeout() || source.is_connect() || source.is_request()
            }
            Error::HttpStatus { status, .. } => *status == 429 || *status >= 500,
            Error::RetriesExhausted { source, .. } => source.is_retryable(),
            // Worth another go later if any provider failed transiently.
            Error::AllProvidersFailed(failures) => {
                failures.iter().any(|f| f.error.is_retryable())
            }
            _ => false,
        }
    }

    /// Whether the failure is a permanent configuration/credential problem
    /// that an operator needs to look at. Useful for deciding when to page.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            Error::MissingConfig(_)
                | Error::InvalidBaseUrl { .. }
                | Error::HttpClientBuild(_)
                | Error::Rejected { .. }
        ) || matches!(self, Error::HttpStatus { status, .. } if *status == 401 || *status == 403)
            // Only page when no provider could possibly have worked.
            || matches!(self, Error::AllProvidersFailed(f)
                if !f.is_empty() && f.iter().all(|p| p.error.is_fatal()))
    }
}

/// One provider's failure inside [`Error::AllProvidersFailed`].
#[derive(Debug)]
pub struct ProviderFailure {
    pub provider: String,
    pub error: Error,
}

impl std::fmt::Display for ProviderFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.provider, self.error)
    }
}

fn summarize(failures: &[ProviderFailure]) -> String {
    failures
        .iter()
        .map(|f| f.to_string())
        .collect::<Vec<_>>()
        .join("; ")
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;
