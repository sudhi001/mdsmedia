//! Gateway credentials and connection settings.

use std::borrow::Cow;
use std::time::Duration;

/// The gateway endpoint used when none is configured.
///
/// This is MDS Media's canonical hostname. Accounts provisioned on a dedicated
/// IP (some are, e.g. `http://XXX.XXX.XXX.XXX/api.php`) must set `base_url`
/// explicitly — those endpoints are plain HTTP and account-specific, so they
/// are never a safe default.
pub const DEFAULT_BASE_URL: &str = "https://mdssend.in/api.php";

fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_string()
}

/// DLT route the message is submitted on.
///
/// `Transactional` is the route used for OTP / service messages in India;
/// `Promotional` is DND-filtered and must not be used for OTPs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum Route {
    #[default]
    Transactional,
    Promotional,
    /// Any other route string the gateway account is provisioned for.
    Custom(String),
}

impl Route {
    /// The literal value sent as the `route` query parameter.
    pub fn as_str(&self) -> &str {
        match self {
            Route::Transactional => "TRANS",
            Route::Promotional => "PROMO",
            Route::Custom(s) => s,
        }
    }
}

impl std::fmt::Display for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<&str> for Route {
    fn from(s: &str) -> Self {
        match s.to_ascii_uppercase().as_str() {
            "TRANS" | "TRANSACTIONAL" => Route::Transactional,
            "PROMO" | "PROMOTIONAL" => Route::Promotional,
            other => Route::Custom(other.to_string()),
        }
    }
}

/// Everything needed to talk to one MDS Media account.
///
/// Construct via [`crate::MdsClient::builder`], or deserialize straight from
/// YAML/JSON with the `serde` feature.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Config {
    /// Gateway endpoint. Defaults to [`DEFAULT_BASE_URL`]; override only for a
    /// dedicated-IP or staging endpoint.
    #[cfg_attr(feature = "serde", serde(default = "default_base_url"))]
    pub base_url: String,
    /// Account username (numeric account id on most MDS deployments).
    pub username: String,
    /// Account API key.
    pub api_key: String,
    /// Approved 6-character sender/header, e.g. `SENDER`.
    pub sender_id: String,
    /// DLT route.
    #[cfg_attr(feature = "serde", serde(default))]
    pub route: Route,
    /// DLT template id (`TID`). Required by most Indian operators.
    #[cfg_attr(feature = "serde", serde(default))]
    pub template_id: Option<String>,
    /// DLT principal entity id (`PEID`).
    #[cfg_attr(feature = "serde", serde(default))]
    pub entity_id: Option<String>,
    /// Country code prepended to bare local numbers, e.g. your country
    /// dialling code as a digit string.
    /// `None` sends the number exactly as given (after cleaning separators).
    #[cfg_attr(feature = "serde", serde(default))]
    pub default_country_code: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            base_url: default_base_url(),
            username: String::new(),
            api_key: String::new(),
            sender_id: String::new(),
            route: Route::default(),
            template_id: None,
            entity_id: None,
            default_country_code: None,
        }
    }
}

impl Config {
    /// Reads every field from environment variables:
    ///
    /// | variable | required |
    /// |---|---|
    /// | `MDS_USERNAME`, `MDS_API_KEY`, `MDS_SENDER_ID` | yes |
    /// | `MDS_BASE_URL` | no — defaults to [`DEFAULT_BASE_URL`] |
    /// | `MDS_ROUTE`, `MDS_TEMPLATE_ID`, `MDS_ENTITY_ID`, `MDS_COUNTRY_CODE` | no |
    pub fn from_env() -> crate::Result<Self> {
        fn req(key: &'static str) -> crate::Result<String> {
            std::env::var(key)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .ok_or(crate::Error::MissingConfig(key))
        }
        fn opt(key: &str) -> Option<String> {
            std::env::var(key).ok().filter(|v| !v.trim().is_empty())
        }

        Ok(Config {
            base_url: opt("MDS_BASE_URL").unwrap_or_else(default_base_url),
            username: req("MDS_USERNAME")?,
            api_key: req("MDS_API_KEY")?,
            sender_id: req("MDS_SENDER_ID")?,
            route: opt("MDS_ROUTE").map(|r| Route::from(r.as_str())).unwrap_or_default(),
            template_id: opt("MDS_TEMPLATE_ID"),
            entity_id: opt("MDS_ENTITY_ID"),
            default_country_code: opt("MDS_COUNTRY_CODE"),
        })
    }

    pub(crate) fn validate(&self) -> crate::Result<()> {
        for (field, value) in [
            ("username", &self.username),
            ("api_key", &self.api_key),
            ("sender_id", &self.sender_id),
        ] {
            if value.trim().is_empty() {
                return Err(crate::Error::MissingConfig(field));
            }
        }
        if self.route.as_str().trim().is_empty() {
            return Err(crate::Error::MissingConfig("route"));
        }
        Ok(())
    }

    /// The endpoint that will actually be called: the configured `base_url`,
    /// or [`DEFAULT_BASE_URL`] when it was left blank.
    pub fn effective_base_url(&self) -> &str {
        let trimmed = self.base_url.trim();
        if trimmed.is_empty() {
            DEFAULT_BASE_URL
        } else {
            trimmed
        }
    }

    /// Normalises a recipient number: strips separators, honours a leading `+`
    /// or `00`, and applies `default_country_code` to bare local numbers.
    ///
    /// Exposed so callers (and the CLI) can validate a list of numbers without
    /// sending anything.
    pub fn normalize(&self, raw: &str) -> crate::Result<String> {
        self.normalize_number(raw).map(|c| c.into_owned())
    }

    pub(crate) fn normalize_number<'a>(&self, raw: &'a str) -> crate::Result<Cow<'a, str>> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(crate::Error::InvalidNumber {
                number: raw.to_string(),
                reason: "empty",
            });
        }

        let had_prefix = trimmed.starts_with('+') || trimmed.starts_with("00");
        let mut digits = String::with_capacity(trimmed.len());
        for ch in trimmed.chars() {
            match ch {
                '0'..='9' => digits.push(ch),
                '+' | '-' | ' ' | '(' | ')' | '.' | '\u{a0}' => {}
                _ => {
                    return Err(crate::Error::InvalidNumber {
                        number: raw.to_string(),
                        reason: "contains non-numeric characters",
                    })
                }
            }
        }
        if had_prefix {
            digits = digits.trim_start_matches("00").to_string();
        }

        // A bare local number gets the configured country code; anything that
        // already carries one (explicit +/00, or simply long enough) is left be.
        if let Some(cc) = self.default_country_code.as_deref() {
            let cc: String = cc.chars().filter(|c| c.is_ascii_digit()).collect();
            if !cc.is_empty() && !had_prefix && !digits.starts_with(&cc) {
                digits.insert_str(0, &cc);
            }
        }

        if digits.len() < 10 || digits.len() > 15 {
            return Err(crate::Error::InvalidNumber {
                number: raw.to_string(),
                reason: "must be 10-15 digits after normalization",
            });
        }
        Ok(Cow::Owned(digits))
    }
}

/// How the client retries transient failures.
///
/// Delays grow geometrically and carry deterministic per-attempt jitter, so a
/// fleet of senders that all trip on the same gateway blip does not retry in
/// lockstep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RetryPolicy {
    /// Retries *after* the first try. `0` disables retrying.
    pub max_retries: u32,
    /// Delay before the first retry.
    pub initial_backoff: Duration,
    /// Upper bound on any single delay.
    pub max_backoff: Duration,
    /// Geometric growth factor, as a percentage (200 = double each time).
    pub multiplier_pct: u32,
    /// Random spread applied to each delay, as a percentage (0 = none).
    pub jitter_pct: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_retries: 3,
            initial_backoff: Duration::from_millis(200),
            max_backoff: Duration::from_secs(5),
            multiplier_pct: 200,
            jitter_pct: 25,
        }
    }
}

impl RetryPolicy {
    /// A policy that never retries.
    pub fn none() -> Self {
        RetryPolicy {
            max_retries: 0,
            ..Default::default()
        }
    }

    /// Delay before `attempt` (1 = first retry).
    pub(crate) fn backoff_for(&self, attempt: u32, seed: u64) -> Duration {
        if attempt == 0 {
            return Duration::ZERO;
        }
        let mut millis = self.initial_backoff.as_millis().max(1) as u64;
        for _ in 1..attempt {
            millis = millis.saturating_mul(self.multiplier_pct.max(100) as u64) / 100;
            if millis >= self.max_backoff.as_millis() as u64 {
                break;
            }
        }
        millis = millis.min(self.max_backoff.as_millis() as u64);

        if self.jitter_pct > 0 {
            // Cheap deterministic spread (splitmix64) — no `rand` dependency.
            let mut x = seed
                .wrapping_add(attempt as u64)
                .wrapping_mul(0x9E37_79B9_7F4A_7C15);
            x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            x ^= x >> 31;
            let spread = millis.saturating_mul(self.jitter_pct.min(100) as u64) / 100;
            if spread > 0 {
                // Jitter is symmetric around the nominal delay.
                let offset = x % (spread * 2 + 1);
                millis = (millis + offset).saturating_sub(spread);
            }
        }
        Duration::from_millis(millis.max(1))
    }
}
