//! The async client.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::stream::{FuturesUnordered, StreamExt};

use crate::config::{Config, RetryPolicy, Route};
use crate::error::{Error, Result};
use crate::message::{Message, Template};
use crate::observer::{Event, Observer};
use crate::response::{self, Parsed, Response};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const DEFAULT_POOL_PER_HOST: usize = 16;
const DEFAULT_CONCURRENCY: usize = 16;
/// Default threshold for emitting `Event::Slow`.
const DEFAULT_SLOW_AFTER: Duration = Duration::from_secs(5);
const USER_AGENT: &str = concat!("mdsmedia-rs/", env!("CARGO_PKG_VERSION"));

/// Result of one send inside a batch. Carries the recipient so failures stay
/// attributable after out-of-order completion.
pub type BatchItem = (String, Result<Response>);

struct Inner {
    name: String,
    config: Config,
    endpoint: url::Url,
    http: reqwest::Client,
    retry: RetryPolicy,
    concurrency: usize,
    slow_after: Duration,
    observer: Option<Arc<dyn Observer>>,
}

impl Inner {
    fn emit(&self, event: Event<'_>) {
        if let Some(observer) = &self.observer {
            observer.on_event(event);
        }
    }
}

/// Async client for the MDS Media SMS gateway.
///
/// Cheap to clone (`Arc` inside) and safe to share across tasks — clones reuse
/// one connection pool, so cloning per task is the intended usage.
///
/// ```no_run
/// # async fn demo() -> mdsmedia::Result<()> {
/// use mdsmedia::{MdsClient, Route};
///
/// let client = MdsClient::builder()
///     .username("XXXXXX")
///     .api_key("secret")
///     .sender_id("SENDER")
///     .route(Route::Transactional)
///     .build()?;
///
/// let resp = client.send("PHONE_NUMBER", "Hello from Rust").await?;
/// println!("accepted: {:?}", resp.message_id);
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct MdsClient {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for MdsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print api_key.
        f.debug_struct("MdsClient")
            .field("name", &self.inner.name)
            .field("endpoint", &self.inner.endpoint.as_str())
            .field("username", &self.inner.config.username)
            .field("sender_id", &self.inner.config.sender_id)
            .field("route", &self.inner.config.route)
            .field("concurrency", &self.inner.concurrency)
            .finish_non_exhaustive()
    }
}

impl MdsClient {
    /// Starts a builder with no fields set.
    pub fn builder() -> MdsClientBuilder {
        MdsClientBuilder::default()
    }

    /// Builds a client from an already-assembled [`Config`], with defaults for
    /// timeouts and retries.
    pub fn from_config(config: Config) -> Result<Self> {
        MdsClientBuilder::default().config(config).build()
    }

    /// Builds a client from `MDS_*` environment variables.
    pub fn from_env() -> Result<Self> {
        Self::from_config(Config::from_env()?)
    }

    /// The account configuration in use.
    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// This client's provider name, used in [`Response::provider`] and events.
    /// Defaults to the endpoint host when not set explicitly.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Default in-flight limit used by [`MdsClient::send_many`].
    pub fn concurrency(&self) -> usize {
        self.inner.concurrency
    }

    /// `host:port` of the gateway — safe to log, unlike the full request URL,
    /// which carries the API key as a query parameter.
    fn endpoint_label(&self) -> String {
        match (self.inner.endpoint.host_str(), self.inner.endpoint.port()) {
            (Some(host), Some(port)) => format!("{host}:{port}"),
            (Some(host), None) => host.to_string(),
            _ => self.inner.name.clone(),
        }
    }

    /// Normalises a recipient number exactly as a send would, without
    /// contacting the gateway. Useful for validating a recipient list upfront.
    pub fn normalize(&self, number: &str) -> Result<String> {
        self.inner.config.normalize(number)
    }

    /// Sends literal text to one recipient.
    pub async fn send(&self, to: impl Into<String>, body: impl Into<String>) -> Result<Response> {
        self.send_message(Message::new(to, body)).await
    }

    /// Renders a DLT template with a single variable and sends it — the OTP path.
    ///
    /// ```no_run
    /// # async fn demo(client: mdsmedia::MdsClient) -> mdsmedia::Result<()> {
    /// use mdsmedia::Template;
    /// let tpl = Template::new("{#var#} is your verification code. Do not share it with anyone.");
    /// client.send_otp("PHONE_NUMBER", "OTP_CODE", &tpl).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn send_otp(
        &self,
        to: impl Into<String>,
        otp: impl AsRef<str>,
        template: &Template,
    ) -> Result<Response> {
        self.send_message(Message::new(to, template.render_one(otp)))
            .await
    }

    /// Sends a fully-specified [`Message`], honouring its per-message overrides.
    pub async fn send_message(&self, message: Message) -> Result<Response> {
        message.validate()?;
        let to = self.inner.config.normalize_number(&message.to)?.into_owned();
        let url = self.build_url(&message, &to)?;

        let started = Instant::now();
        let mut attempt: u32 = 0;
        let seed = seed_from(&to);
        let name = self.inner.name.as_str();

        loop {
            attempt += 1;
            self.inner.emit(Event::Attempt {
                provider: name,
                to: &to,
                attempt,
            });

            match self.execute(&url).await {
                Ok((status, body)) => match response::parse_body(&body) {
                    Parsed::Accepted { message_id } => {
                        let elapsed = started.elapsed();
                        if elapsed > self.inner.slow_after {
                            self.inner.emit(Event::Slow {
                                provider: name,
                                to: &to,
                                elapsed,
                            });
                        }
                        let response = Response {
                            provider: self.inner.name.clone(),
                            message_id,
                            to,
                            status,
                            raw: body,
                            elapsed,
                            attempts: attempt,
                            reference: message.reference,
                        };
                        self.inner.emit(Event::Accepted {
                            provider: name,
                            response: &response,
                        });
                        return Ok(response);
                    }
                    Parsed::Rejected { reason } => {
                        // A rejection is the gateway's considered answer;
                        // retrying it only burns quota.
                        let err = Error::Rejected { reason, raw: body };
                        self.inner.emit(Event::Failed {
                            provider: name,
                            to: &to,
                            error: &err,
                        });
                        return Err(err);
                    }
                },
                Err(err) => {
                    let retries_left = attempt <= self.inner.retry.max_retries;
                    if !err.is_retryable() || !retries_left {
                        let err = if attempt > 1 {
                            Error::RetriesExhausted {
                                attempts: attempt,
                                source: Box::new(err),
                            }
                        } else {
                            err
                        };
                        self.inner.emit(Event::Failed {
                            provider: name,
                            to: &to,
                            error: &err,
                        });
                        return Err(err);
                    }
                    let delay = self.inner.retry.backoff_for(attempt, seed);
                    self.inner.emit(Event::Retrying {
                        provider: name,
                        to: &to,
                        attempt,
                        delay,
                        error: &err,
                    });
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    /// Sends many messages concurrently, capped at the client's concurrency
    /// limit (see [`MdsClientBuilder::concurrency`]).
    ///
    /// Never short-circuits: every message gets an outcome, returned in input
    /// order even though sends complete out of order. One bad recipient cannot
    /// sink the rest of the batch.
    ///
    /// ```no_run
    /// # async fn demo(client: mdsmedia::MdsClient) {
    /// use mdsmedia::Message;
    /// let msgs = vec![
    ///     Message::new("PHONE_NUMBER", "one"),
    ///     Message::new("ANOTHER_PHONE_NUMBER", "two"),
    /// ];
    /// for (to, result) in client.send_many(msgs).await {
    ///     match result {
    ///         Ok(r) => println!("{to} ok {:?}", r.message_id),
    ///         Err(e) => eprintln!("{to} failed: {e}"),
    ///     }
    /// }
    /// # }
    /// ```
    pub async fn send_many(&self, messages: Vec<Message>) -> Vec<BatchItem> {
        self.send_many_with_concurrency(messages, self.inner.concurrency)
            .await
    }

    /// [`send_many`](Self::send_many) with a per-call concurrency override.
    pub async fn send_many_with_concurrency(
        &self,
        messages: Vec<Message>,
        concurrency: usize,
    ) -> Vec<BatchItem> {
        let total = messages.len();
        let mut slots: Vec<Option<BatchItem>> = (0..total).map(|_| None).collect();
        if total == 0 {
            return Vec::new();
        }
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

        // Every slot was filled: the loop drains exactly `total` futures.
        slots.into_iter().flatten().collect()
    }

    async fn send_indexed(&self, idx: usize, message: Message) -> (usize, String, Result<Response>) {
        let to = message.to.clone();
        let result = self.send_message(message).await;
        (idx, to, result)
    }

    /// Performs one HTTP round-trip, mapping transport and status failures.
    ///
    /// Every error is stripped of its URL before it escapes: reqwest embeds the
    /// full request URL in its `Display`, and ours carries `apikey` in the
    /// query string. Without this, one logged transport error leaks the
    /// account credential. [`Error::Transport`] carries `endpoint` instead, so
    /// the host is still identifiable.
    async fn execute(&self, url: &url::Url) -> Result<(u16, String)> {
        let started = Instant::now();
        let redact = |source: reqwest::Error| Error::Transport {
            elapsed: started.elapsed(),
            endpoint: self.endpoint_label(),
            source: source.without_url(),
        };

        let resp = self.inner.http.get(url.clone()).send().await.map_err(redact)?;

        let status = resp.status();
        let body = resp.text().await.map_err(redact)?;

        if !status.is_success() {
            return Err(Error::HttpStatus {
                status: status.as_u16(),
                body: truncate(&body, 512),
            });
        }
        Ok((status.as_u16(), body))
    }

    /// Builds the request URL. Every value goes through proper percent-encoding
    /// — an unescaped `&` or `#` in a message body would otherwise silently
    /// corrupt the query string.
    fn build_url(&self, message: &Message, to: &str) -> Result<url::Url> {
        let cfg = &self.inner.config;
        let mut url = self.inner.endpoint.clone();
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("username", &cfg.username);
            q.append_pair("apikey", &cfg.api_key);
            q.append_pair(
                "senderid",
                message.sender_id.as_deref().unwrap_or(&cfg.sender_id),
            );
            q.append_pair(
                "route",
                message.route.as_ref().unwrap_or(&cfg.route).as_str(),
            );
            q.append_pair("text", &message.body);
            q.append_pair("mobile", to);

            if let Some(tid) = message
                .template_id
                .as_deref()
                .or(cfg.template_id.as_deref())
            {
                q.append_pair("TID", tid);
            }
            if let Some(peid) = cfg.entity_id.as_deref() {
                q.append_pair("PEID", peid);
            }
        }
        Ok(url)
    }
}

/// Stable per-recipient seed, so retry jitter differs between recipients but
/// stays reproducible for a given one.
fn seed_from(s: &str) -> u64 {
    // FNV-1a
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Builder for [`MdsClient`].
pub struct MdsClientBuilder {
    config: Config,
    retry: RetryPolicy,
    timeout: Duration,
    connect_timeout: Duration,
    pool_max_idle_per_host: usize,
    pool_idle_timeout: Duration,
    concurrency: usize,
    user_agent: String,
    name: Option<String>,
    slow_after: Duration,
    observer: Option<Arc<dyn Observer>>,
}

impl Default for MdsClientBuilder {
    fn default() -> Self {
        MdsClientBuilder {
            config: Config::default(),
            retry: RetryPolicy::default(),
            timeout: DEFAULT_TIMEOUT,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            pool_max_idle_per_host: DEFAULT_POOL_PER_HOST,
            pool_idle_timeout: DEFAULT_POOL_IDLE_TIMEOUT,
            concurrency: DEFAULT_CONCURRENCY,
            user_agent: USER_AGENT.to_string(),
            name: None,
            slow_after: DEFAULT_SLOW_AFTER,
            observer: None,
        }
    }
}

impl std::fmt::Debug for MdsClientBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MdsClientBuilder")
            .field("name", &self.name)
            .field("timeout", &self.timeout)
            .field("concurrency", &self.concurrency)
            .field("has_observer", &self.observer.is_some())
            .finish_non_exhaustive()
    }
}

impl MdsClientBuilder {
    /// Names this provider, e.g. `"mds1"`. Surfaces in [`Response::provider`]
    /// and in every [`Event`]. Defaults to the endpoint host.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Registers an observability hook. See [`Observer`].
    pub fn observer<O: Observer>(mut self, observer: O) -> Self {
        self.observer = Some(Arc::new(observer));
        self
    }

    /// Shares an already-constructed observer across several clients — the
    /// usual case when a [`crate::FailoverClient`] feeds one metrics sink.
    pub fn shared_observer(mut self, observer: Arc<dyn Observer>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Emits [`Event::Slow`] for sends exceeding this. Default 5s.
    pub fn slow_after(mut self, threshold: Duration) -> Self {
        self.slow_after = threshold;
        self
    }

    /// Replaces the whole account config at once.
    pub fn config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    /// Overrides the gateway endpoint. Optional — defaults to
    /// [`crate::DEFAULT_BASE_URL`]; set this only for a dedicated-IP or
    /// staging endpoint.
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.config.base_url = url.into();
        self
    }

    pub fn username(mut self, username: impl Into<String>) -> Self {
        self.config.username = username.into();
        self
    }

    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.config.api_key = key.into();
        self
    }

    pub fn sender_id(mut self, sender: impl Into<String>) -> Self {
        self.config.sender_id = sender.into();
        self
    }

    pub fn route(mut self, route: impl Into<Route>) -> Self {
        self.config.route = route.into();
        self
    }

    /// DLT template id (`TID`).
    pub fn template_id(mut self, tid: impl Into<String>) -> Self {
        self.config.template_id = Some(tid.into());
        self
    }

    /// DLT principal entity id (`PEID`).
    pub fn entity_id(mut self, peid: impl Into<String>) -> Self {
        self.config.entity_id = Some(peid.into());
        self
    }

    /// Country code prepended to bare local numbers, e.g. your country
    /// dialling code as a digit string.
    pub fn default_country_code(mut self, cc: impl Into<String>) -> Self {
        self.config.default_country_code = Some(cc.into());
        self
    }

    pub fn retry(mut self, policy: RetryPolicy) -> Self {
        self.retry = policy;
        self
    }

    /// Total per-attempt request timeout. Default 20s.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// TCP+TLS connect timeout. Default 10s.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Idle keep-alive connections held per gateway host. Default 16.
    pub fn pool_max_idle_per_host(mut self, n: usize) -> Self {
        self.pool_max_idle_per_host = n;
        self
    }

    pub fn pool_idle_timeout(mut self, timeout: Duration) -> Self {
        self.pool_idle_timeout = timeout;
        self
    }

    /// Default in-flight sends for [`MdsClient::send_many`]. Default 16.
    pub fn concurrency(mut self, n: usize) -> Self {
        self.concurrency = n.max(1);
        self
    }

    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = ua.into();
        self
    }

    /// Validates the config and constructs the client.
    pub fn build(self) -> Result<MdsClient> {
        self.config.validate()?;

        let base_url = self.config.effective_base_url().to_string();
        let endpoint = url::Url::parse(&base_url).map_err(|source| Error::InvalidBaseUrl {
            url: base_url,
            source,
        })?;

        let http = reqwest::Client::builder()
            .timeout(self.timeout)
            .connect_timeout(self.connect_timeout)
            .pool_max_idle_per_host(self.pool_max_idle_per_host)
            .pool_idle_timeout(self.pool_idle_timeout)
            .user_agent(self.user_agent)
            .build()
            .map_err(Error::HttpClientBuild)?;

        let name = self.name.unwrap_or_else(|| {
            endpoint
                .host_str()
                .map(str::to_string)
                .unwrap_or_else(|| "mds".to_string())
        });

        Ok(MdsClient {
            inner: Arc::new(Inner {
                name,
                config: self.config,
                endpoint,
                http,
                retry: self.retry,
                concurrency: self.concurrency,
                slow_after: self.slow_after,
                observer: self.observer,
            }),
        })
    }
}
