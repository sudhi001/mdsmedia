//! Synchronous wrapper, for callers with no async runtime.
//!
//! Enable with the `blocking` feature. Each client owns a small multi-thread
//! runtime, so [`BlockingMdsClient::send_many`] still runs sends concurrently.

use std::sync::Arc;

use crate::{Config, Message, MdsClient, MdsClientBuilder, Response, Result, Template};

/// Blocking facade over [`MdsClient`]. Cheap to clone; clones share one runtime
/// and one connection pool.
#[derive(Clone, Debug)]
pub struct BlockingMdsClient {
    inner: MdsClient,
    runtime: Arc<tokio::runtime::Runtime>,
}

impl BlockingMdsClient {
    /// Wraps an existing async client. The runtime is created here, so this
    /// must not be called from inside an async context.
    pub fn new(inner: MdsClient) -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        Ok(BlockingMdsClient {
            inner,
            runtime: Arc::new(runtime),
        })
    }

    /// Builds from a [`Config`].
    pub fn from_config(config: Config) -> Result<Self> {
        let client = MdsClient::from_config(config)?;
        Self::new(client).map_err(crate::Error::Runtime)
    }

    /// Builds from `MDS_*` environment variables.
    pub fn from_env() -> Result<Self> {
        Self::from_config(Config::from_env()?)
    }

    /// Same builder as the async client; call [`MdsClientBuilder::build`] then
    /// [`BlockingMdsClient::new`].
    pub fn builder() -> MdsClientBuilder {
        MdsClient::builder()
    }

    /// The underlying async client, for mixed sync/async callers.
    pub fn async_client(&self) -> &MdsClient {
        &self.inner
    }

    pub fn send(&self, to: impl Into<String>, body: impl Into<String>) -> Result<Response> {
        self.runtime.block_on(self.inner.send(to, body))
    }

    pub fn send_otp(
        &self,
        to: impl Into<String>,
        otp: impl AsRef<str>,
        template: &Template,
    ) -> Result<Response> {
        self.runtime.block_on(self.inner.send_otp(to, otp, template))
    }

    pub fn send_message(&self, message: Message) -> Result<Response> {
        self.runtime.block_on(self.inner.send_message(message))
    }

    pub fn send_many(&self, messages: Vec<Message>) -> Vec<crate::BatchItem> {
        self.runtime.block_on(self.inner.send_many(messages))
    }
}
