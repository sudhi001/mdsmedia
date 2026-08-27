//! # mdsmedia
//!
//! A small, concurrent client for the **MDS Media** (`mdssend.in`) SMS HTTP
//! gateway — the `api.php` endpoint used for OTP and transactional SMS in
//! India, including DLT `TID`/`PEID` parameters.
//!
//! ## Unofficial
//!
//! This is an **unofficial**, community-maintained client. It is not affiliated
//! with, endorsed by, or supported by MDS Media, and the name is used only to
//! describe what the crate talks to. There is no published API specification;
//! behaviour here is derived from observing live responses, so a deployment may
//! answer in a shape this crate has not seen. [`Response::raw`] always carries
//! the verbatim body for exactly that reason. Verify against your own account
//! before relying on it, and treat gateway behaviour as subject to change
//! without notice.
//!
//! Maintained by Sudhi S <support@sudhi.in> —
//! <https://github.com/sudhi001/mdsmedia>. Report problems there, not to
//! MDS Media.
//!
//! ## Why this exists
//!
//! The gateway is a single GET endpoint, so most integrations end up as a
//! hand-rolled `format!` of a URL. That works until it doesn't: unescaped `&`
//! in a message body silently truncates the text, HTTP 200 is returned for
//! `ERROR: Invalid API Key`, a *successful* send reports
//! `Total Invalid Numbers : 0` (which a substring search reads as failure),
//! and every caller reinvents retries. This crate handles those and stays out
//! of the way otherwise.
//!
//! ## Quick start
//!
//! ```no_run
//! use mdsmedia::{MdsClient, Route, Template};
//!
//! # async fn demo() -> mdsmedia::Result<()> {
//! let client = MdsClient::builder()
//!     .username("XXXXXX")
//!     .api_key("secret")
//!     .sender_id("SENDER")
//!     .route(Route::Transactional)
//!     // .base_url(...) is optional — defaults to DEFAULT_BASE_URL
//!     .template_id("XXXXXXXXXXXXXXXXXXX")   // DLT TID
//!     .entity_id("XXXXXXXXXXXXXXXXXXX")     // DLT PEID
//!     .default_country_code("XX")
//!     .build()?;
//!
//! let tpl = Template::new("{#var#} is your verification code. Do not share it with anyone.");
//! let resp = client.send_otp("PHONE_NUMBER", "OTP_CODE", &tpl).await?;
//! println!("accepted in {:?}: {:?}", resp.elapsed, resp.message_id);
//! # Ok(())
//! # }
//! ```
//!
//! ## Failover
//!
//! [`FailoverClient`] tries several provider accounts in priority order, so a
//! second provisioned account is a fallback rather than dead config. See its
//! docs for what does and does not trigger a switch.
//!
//! ## Observability
//!
//! The library never logs or alerts. Register an [`Observer`] to see attempts,
//! retries, slow sends, failures, and failover hops, and route them wherever
//! your service already sends telemetry.
//!
//! ## Concurrency
//!
//! [`MdsClient`] is `Clone + Send + Sync` and clones share one connection pool,
//! so the intended pattern is to build once and clone into tasks.
//! [`MdsClient::send_many`] fans a batch out under a bounded in-flight limit and
//! returns one outcome per input message, in input order — a single bad
//! recipient never sinks the batch.
//!
//! ## Features
//!
//! | feature | default | effect |
//! |---|---|---|
//! | `rustls-tls` | yes | TLS via rustls (no OpenSSL) |
//! | `native-tls` | no | TLS via the platform stack instead |
//! | `json` | yes | parse JSON gateway responses |
//! | `serde` | no | `Serialize`/`Deserialize` on [`Config`] and [`Response`] |
//! | `blocking` | no | [`blocking::BlockingMdsClient`] for sync callers |
//! | `cli` | no | the `mdsmedia` command-line tool |

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

mod client;
mod config;
mod failover;
mod error;
mod message;
mod observer;
mod response;

#[cfg(feature = "blocking")]
pub mod blocking;

pub use client::{BatchItem, MdsClient, MdsClientBuilder};
pub use error::ProviderFailure;
pub use failover::FailoverClient;
pub use observer::{Event, Observer};
pub use config::{Config, RetryPolicy, Route, DEFAULT_BASE_URL};
pub use error::{Error, Result};
pub use message::{Message, Template, MAX_BODY_LEN, VAR};
pub use response::Response;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cfg() -> Config {
        Config {
            username: "123456".into(),
            api_key: "k".into(),
            sender_id: "SENDER".into(),
            default_country_code: Some("91".into()),
            ..Config::default()
        }
    }

    #[test]
    fn template_renders_positionally() {
        let t = Template::new("{#var#} is your OTP, valid {#var#} min.");
        assert_eq!(t.placeholders(), 2);
        assert_eq!(t.render(&["1234", "5"]), "1234 is your OTP, valid 5 min.");
        assert_eq!(t.render_one("1234"), "1234 is your OTP, valid {#var#} min.");
    }

    #[test]
    fn numbers_are_normalized() {
        let c = cfg();
        assert_eq!(c.normalize_number("9995551212").unwrap(), "919995551212");
        assert_eq!(c.normalize_number("+91 99955-51212").unwrap(), "919995551212");
        assert_eq!(c.normalize_number("00919995551212").unwrap(), "919995551212");
        assert_eq!(c.normalize_number("919995551212").unwrap(), "919995551212");
    }

    #[test]
    fn bad_numbers_are_rejected_before_any_request() {
        let c = cfg();
        assert!(c.normalize_number("").is_err());
        assert!(c.normalize_number("12345").is_err());
        assert!(c.normalize_number("99955-ABCD").is_err());
    }

    #[test]
    fn no_country_code_leaves_number_alone() {
        let c = Config {
            default_country_code: None,
            ..cfg()
        };
        assert_eq!(c.normalize_number("9995551212").unwrap(), "9995551212");
    }

    #[test]
    fn missing_credentials_fail_at_build_time() {
        let err = MdsClient::builder().username("u").build().unwrap_err();
        assert!(matches!(err, Error::MissingConfig("api_key")));
        assert!(err.is_fatal());
    }

    #[test]
    fn bad_base_url_is_rejected() {
        let err = MdsClient::builder()
            .base_url("not a url")
            .username("u")
            .api_key("k")
            .sender_id("S")
            .build()
            .unwrap_err();
        assert!(matches!(err, Error::InvalidBaseUrl { .. }));
    }

    #[test]
    fn empty_body_is_rejected() {
        assert!(Message::new("9995551212", "   ").validate().is_err());
        assert!(Message::new("9995551212", "x".repeat(MAX_BODY_LEN + 1))
            .validate()
            .is_err());
    }

    #[test]
    fn backoff_grows_and_stays_bounded() {
        let p = RetryPolicy {
            max_retries: 5,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(800),
            multiplier_pct: 200,
            jitter_pct: 0,
        };
        assert_eq!(p.backoff_for(0, 1), Duration::ZERO);
        assert_eq!(p.backoff_for(1, 1), Duration::from_millis(100));
        assert_eq!(p.backoff_for(2, 1), Duration::from_millis(200));
        assert_eq!(p.backoff_for(3, 1), Duration::from_millis(400));
        assert_eq!(p.backoff_for(4, 1), Duration::from_millis(800));
        assert_eq!(p.backoff_for(9, 1), Duration::from_millis(800));
    }

    #[test]
    fn jitter_stays_within_the_configured_band() {
        let p = RetryPolicy {
            jitter_pct: 25,
            initial_backoff: Duration::from_millis(1000),
            max_backoff: Duration::from_secs(60),
            ..RetryPolicy::default()
        };
        for seed in 0..500u64 {
            let d = p.backoff_for(1, seed).as_millis() as i64;
            assert!((750..=1250).contains(&d), "jitter out of band: {d}ms");
        }
    }

    #[test]
    fn retryability_matches_intent() {
        assert!(Error::HttpStatus {
            status: 503,
            body: String::new()
        }
        .is_retryable());
        assert!(Error::HttpStatus {
            status: 429,
            body: String::new()
        }
        .is_retryable());
        assert!(!Error::HttpStatus {
            status: 401,
            body: String::new()
        }
        .is_retryable());
        assert!(!Error::Rejected {
            reason: "bad key".into(),
            raw: String::new()
        }
        .is_retryable());
    }

    #[test]
    fn debug_output_never_leaks_the_api_key() {
        let client = MdsClient::from_config(Config {
            api_key: "super-secret-key".into(),
            ..cfg()
        })
        .unwrap();
        assert!(!format!("{client:?}").contains("super-secret-key"));
    }

    #[test]
    fn base_url_is_optional_and_defaults() {
        // A client built without base_url still resolves to the canonical host.
        let client = MdsClient::builder()
            .username("u")
            .api_key("k")
            .sender_id("S")
            .build()
            .unwrap();
        assert_eq!(client.config().effective_base_url(), DEFAULT_BASE_URL);
        assert_eq!(Config::default().base_url, DEFAULT_BASE_URL);
        assert!(DEFAULT_BASE_URL.starts_with("https://"));
    }

    #[test]
    fn blank_base_url_falls_back_to_the_default() {
        // Deserialized or hand-built configs may carry an empty string; that
        // must resolve to the default rather than failing to parse as a URL.
        let config = Config {
            base_url: "   ".into(),
            ..cfg()
        };
        assert_eq!(config.effective_base_url(), DEFAULT_BASE_URL);
        assert!(MdsClient::from_config(config).is_ok());
    }

    #[test]
    fn an_explicit_base_url_still_wins() {
        let client = MdsClient::builder()
            .username("u")
            .api_key("k")
            .sender_id("S")
            .base_url("http://XXX.XXX.XXX.XXX/api.php")
            .build()
            .unwrap();
        assert_eq!(
            client.config().effective_base_url(),
            "http://XXX.XXX.XXX.XXX/api.php"
        );
    }

    #[test]
    fn routes_round_trip() {
        assert_eq!(Route::from("trans").as_str(), "TRANS");
        assert_eq!(Route::from("Promotional").as_str(), "PROMO");
        assert_eq!(Route::from("OTP").as_str(), "OTP");
    }
}
