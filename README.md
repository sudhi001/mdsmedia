# mdsmedia

[![release](https://img.shields.io/github/v/release/sudhi001/mdsmedia?sort=semver)](https://github.com/sudhi001/mdsmedia/releases)
[![license](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![docs](https://img.shields.io/badge/docs-wiki-informational)](https://github.com/sudhi001/mdsmedia/wiki)

A small, concurrent Rust client for the **MDS Media** (`mdssend.in`) SMS HTTP
gateway — the `api.php` endpoint used for OTP and transactional SMS in India,
including DLT `TID` / `PEID` parameters.

> ### ⚠️ Unofficial
>
> **This is an unofficial, community-maintained library. It is not affiliated
> with, endorsed by, supported by, or in any way connected to MDS Media.** The
> name is used solely to describe the gateway the crate talks to; all
> trademarks belong to their respective owners.
>
> There is no published API specification for this gateway. Everything here —
> the parameter names, the response shapes, the failure signalling — is derived
> from observing live traffic against real accounts. A deployment may well
> answer in a shape this crate has not seen, which is why `Response::raw`
> always carries the verbatim body.
>
> **Verify against your own account before relying on it.** Gateway behaviour
> can change without notice. Provided as-is, with no warranty (see LICENSE).
>
> Maintained by **Sudhi S** &lt;support@sudhi.in&gt;. Report problems here, not
> to MDS Media.

Extracted from a production Go service so the same gateway can be reused across
projects without re-deriving the integration each time.

---

## Why not just `format!` a URL?

That is what most MDS integrations do, and it holds up until it doesn't:

| Problem | What this crate does |
|---|---|
| An unescaped `&`, `#` or `%` in the body silently truncates the message | Every parameter is percent-encoded via `Url::query_pairs_mut` |
| The gateway returns **HTTP 200** for `ERROR: Invalid API Key` | 2xx bodies are parsed; rejections become `Error::Rejected` |
| Every caller reinvents retries — and often retries bad credentials | Exponential backoff with jitter, and only for genuinely transient failures |
| Numbers arrive as `+XX XXXXX-XXXXX`, `0XXXXXXXXXX`, `XXXXXXXXXX` | One normalizer, applied before any network call |
| Batch sends run serially, or fan out unbounded | `send_many` under a bounded in-flight limit |
| A second provisioned account sits unused while the primary is down | `FailoverClient` tries providers in order |
| The transport owns logging and alerting, so it can't be reused | `Observer` hook; the library never logs or emails |
| Transport errors embed the full URL — including `apikey` — in logs | URLs are stripped from every error before it escapes |

## Install

### Command-line tool

```bash
brew install sudhi001/tap/mdsmedia
```

Builds from source, so it works on **macOS and Linux** alike. Homebrew supplies
the Rust toolchain — you do not need one installed. If Homebrew prompts you to
trust the tap, run `brew trust sudhi001/tap` and install again.

Without Homebrew:

```bash
cargo install --git https://github.com/sudhi001/mdsmedia --tag v0.1.0 --features cli
```

A prebuilt macOS arm64 binary is attached to each
[release](https://github.com/sudhi001/mdsmedia/releases).

### Library

```toml
[dependencies]
mdsmedia = { git = "https://github.com/sudhi001/mdsmedia", tag = "v0.1.0" }
```

Pin the tag. The gateway has no published specification, so parsing may need to
change as new response shapes are observed; tracking `main` means those changes
arrive unannounced.

## Usage

```rust
use mdsmedia::{MdsClient, Route, Template};

let client = MdsClient::builder()
    .username("XXXXXX")
    .api_key(std::env::var("MDS_API_KEY")?)
    .sender_id("SENDER")
    .route(Route::Transactional)
    .template_id("XXXXXXXXXXXXXXXXXXX")   // DLT TID
    .entity_id("XXXXXXXXXXXXXXXXXXX")     // DLT PEID
    .default_country_code("XX")            // your country dialling code
    // .base_url(...) is optional — defaults to https://mdssend.in/api.php.
    // Set it only for a dedicated-IP or staging endpoint.
    .build()?;

let tpl = Template::new("{#var#} is your verification code. Do not share it with anyone.");
let resp = client.send_otp("PHONE_NUMBER", "OTP_CODE", &tpl).await?;

println!("accepted in {:?}: {:?}", resp.elapsed, resp.message_id);
```

Only `username`, `api_key` and `sender_id` are required. Or straight from the
environment — `MDS_USERNAME`, `MDS_API_KEY` and `MDS_SENDER_ID` are required,
`MDS_BASE_URL`, `MDS_ROUTE`, `MDS_TEMPLATE_ID`, `MDS_ENTITY_ID` and
`MDS_COUNTRY_CODE` are optional:

```rust
let client = MdsClient::from_env()?;
```

### The endpoint

`base_url` defaults to `mdsmedia::DEFAULT_BASE_URL` — `https://mdssend.in/api.php`,
MDS Media's canonical host. Accounts provisioned on a dedicated IP need it set
explicitly:

```rust
.base_url("http://XXX.XXX.XXX.XXX/api.php")
```

Those endpoints are plain HTTP and account-specific, which is exactly why they
are not the default. `Config::effective_base_url()` reports what will actually
be called, and a blank or whitespace-only value falls back to the default rather
than failing to parse.

### Concurrency

`MdsClient` is `Clone + Send + Sync`, and clones share one connection pool — so
build once and clone into tasks. For a batch, `send_many` bounds how many sends
are in flight and returns **one outcome per input message, in input order**:

```rust
use mdsmedia::Message;

let messages = vec![
    Message::new("PHONE_NUMBER", "Maintenance tonight 11pm-1am."),
    Message::new("ANOTHER_PHONE_NUMBER", "Maintenance tonight 11pm-1am.").reference("ops-lead"),
];

for (to, result) in client.send_many(messages).await {
    match result {
        Ok(r)  => println!("OK   {to} id={:?}", r.message_id),
        Err(e) => println!("FAIL {to} {e}"),
    }
}
```

A single invalid recipient never sinks the batch — it fails locally, before any
request is issued, and its neighbours still go out.

### Failover

MDS accounts are commonly provisioned in pairs — a canonical HTTPS host plus a
dedicated IP over plain HTTP. Integrations typically select one at startup:

```go
if primary == "b" { use_b() } else { use_a() }
```

Two working accounts, one ever used. When the selected endpoint drops — and a
bare IP over plain HTTP has no TLS and no DNS failover — every message fails
while a healthy account sits idle. `FailoverClient` is that missing `else`:

```rust
use mdsmedia::{FailoverClient, MdsClient};

let primary  = MdsClient::builder().name("mds1")
    .base_url("http://XXX.XXX.XXX.XXX/api.php")
    .username("XXXXXX").api_key(&key).sender_id("SENDER").build()?;

let fallback = MdsClient::builder().name("mds")   // defaults to mdssend.in
    .username("XXXXXX").api_key(&key).sender_id("SENDER").build()?;

let client = FailoverClient::new(vec![primary, fallback])?;
let resp = client.send_otp("PHONE_NUMBER", "OTP_CODE", &tpl).await?;
println!("served by {}", resp.provider);
```

It fails over on transport errors, 5xx, and gateway rejections — a rejection is
usually credential- or template-specific, and the next account has different
ones. It does **not** fail over on a malformed number or an oversized body:
those fail identically everywhere, so trying again just wastes a round-trip.
When every provider fails you get `Error::AllProvidersFailed`, carrying each
provider's own error rather than flattening to the last one.

One caveat: each account has its own DLT `TID`, so a template must be registered
against **every** provider in the chain, or the fallback will be accepted by the
gateway and then dropped by the carrier.

### Observability

The library never logs, emails, or alerts — it reports, and you decide. This
policy commonly ends up *inside* the transport (an error-email call, a throttle
to stop outages flooding the inbox, a suppressed-failure counter), which makes
the send path impossible to reuse without inheriting the alerting.

```rust
use mdsmedia::Event;

let client = MdsClient::builder()
    .username("XXXXXX").api_key(&key).sender_id("SENDER")
    .slow_after(Duration::from_secs(5))     // warn on degraded delivery
    .observer(|event: Event<'_>| match event {
        Event::Accepted { response, .. } => metrics.sent.inc(),
        Event::Retrying { provider, delay, .. } => tracing::warn!(%provider, ?delay, "retrying"),
        Event::Slow { elapsed, .. } => metrics.slow.inc(),
        Event::Failed { error, .. } => alert(error),
        _ => {}
    })
    .build()?;
```

`Event::Failed` fires **once per send**, not once per attempt — emitting
per-attempt is what drives integrations to bolt a throttle onto their alerting
in the first place. Observers are called
inline on the send path, so don't block in them: bump a counter or push to a
channel.

### Credentials in errors

`reqwest` puts the full request URL in its error `Display`, and the MDS API
takes `apikey` as a query parameter — so one logged transport error leaks the
account key. Every error is stripped of its URL before it escapes; `Error::Transport`
carries the host instead:

```
transport error contacting XXX.XXX.XXX.XXX after 583ms: error sending request
```

A test asserts the key appears in no error's `Display`, `Debug`, or source chain.
`MdsClient`'s own `Debug` omits it too.

### Errors

`Error` distinguishes failures you should retry from failures you should page on:

```rust
match client.send(number, body).await {
    Ok(resp) => { /* gateway accepted it */ }
    Err(e) if e.is_fatal()     => alert_ops(&e),   // bad key, bad sender, bad URL
    Err(e) if e.is_retryable() => enqueue_retry(), // transport, 429, 5xx
    Err(e)                     => log::warn!("{e}"),
}
```

Note that acceptance means the *gateway* took the message, not that a handset
received it. MDS reports final delivery only via DLR callbacks.

### Retries

```rust
use mdsmedia::RetryPolicy;
use std::time::Duration;

.retry(RetryPolicy {
    max_retries: 3,
    initial_backoff: Duration::from_millis(200),
    max_backoff: Duration::from_secs(5),
    multiplier_pct: 200,   // double each attempt
    jitter_pct: 25,        // ±25% spread, so a fleet doesn't retry in lockstep
})
```

Gateway *rejections* are never retried — retrying `Invalid API Key` only burns
quota. Jitter is derived from the recipient number, so it is reproducible per
number without pulling in a RNG dependency.

### Synchronous callers

```toml
mdsmedia = { version = "0.1", features = ["blocking"] }
```

```rust
use mdsmedia::blocking::BlockingMdsClient;

let client = BlockingMdsClient::from_env()?;
client.send("PHONE_NUMBER", "hello")?;
```

`send_many` on the blocking client still runs sends concurrently.

---

## CLI

For testing an account end to end without writing code:

```bash
brew install sudhi001/tap/mdsmedia
```

or, from a clone: `cargo build --release --features cli`.

Credentials come from the same `MDS_*` variables, a `--env-file`, or flags.

```bash
# Validate config without sending anything
mdsmedia --env-file mds.env check

# Check how numbers will be normalized
mdsmedia --env-file mds.env normalize PHONE_NUMBER "+XX XXXXX-XXXXX" TOO_SHORT

# Send an OTP
mdsmedia --env-file mds.env otp --to PHONE_NUMBER --code OTP_CODE

# Send text to several numbers, as JSON, at concurrency 4
mdsmedia --env-file mds.env --json send -t PHONE_NUMBER,ANOTHER_PHONE_NUMBER -m "Hello" -c 4

# Build the request and print it, without contacting the gateway
mdsmedia --env-file mds.env --dry-run send -t PHONE_NUMBER -m "Hello"

# Bodies with quotes or newlines: pipe them in
echo "Pay & save 10% #now" | mdsmedia --env-file mds.env send -t PHONE_NUMBER -m -

# One body to a file of numbers (blank lines and `#` comments skipped)
mdsmedia --env-file mds.env bulk -f numbers.txt -m "Maintenance tonight" -c 16

# Failover, with the lifecycle traced to stderr
mdsmedia --env-file mds.env \
  --fallback-url https://mdssend.in/api.php --trace \
  otp --to PHONE_NUMBER --code OTP_CODE
```

`--trace` prints attempts, retries, slow sends, and failover hops to stderr,
leaving stdout parseable:

```
sending 1 message(s) at concurrency 8 via primary -> fallback...
  [primary] attempt 1 -> PHONE_NUMBER
  [primary] attempt 1 failed (transport error contacting XXX.XXX.XXX.XXX ...); retrying in 191ms
  [primary] -> [fallback]: giving up after 2 attempt(s): ...
  [fallback] attempt 1 -> PHONE_NUMBER
OK    PHONE_NUMBER  via=fallback  id=MESSAGE_ID  89ms  attempt(s)=1
```

Exit code is `0` only if every message was accepted, so it drops into CI and
health checks directly. `check` prints the API key masked and flags whether the
endpoint is the built-in default.

`mds.env`:

```
MDS_USERNAME=XXXXXX
MDS_API_KEY=...
MDS_SENDER_ID=SENDER
# Optional — omit for https://mdssend.in/api.php
# MDS_BASE_URL=http://XXX.XXX.XXX.XXX/api.php
MDS_ROUTE=TRANS
MDS_TEMPLATE_ID=XXXXXXXXXXXXXXXXXXX
MDS_ENTITY_ID=XXXXXXXXXXXXXXXXXXX
MDS_COUNTRY_CODE=XX
```

Real environment variables win over the file, so an exported secret is never
clobbered by a checked-in default.

---

## Documentation

Full reference lives in the [wiki](https://github.com/sudhi001/mdsmedia/wiki):

| | |
|---|---|
| [Installation](https://github.com/sudhi001/mdsmedia/wiki/Installation) | Feature flags, MSRV, building from a clone |
| [Configuration](https://github.com/sudhi001/mdsmedia/wiki/Configuration) | Credentials, endpoints, DLT ids, number normalization |
| [Sending SMS](https://github.com/sudhi001/mdsmedia/wiki/Sending-SMS) | Single sends, templates, batches, concurrency |
| [Failover](https://github.com/sudhi001/mdsmedia/wiki/Failover) | Ordered failover across provider accounts |
| [Observability](https://github.com/sudhi001/mdsmedia/wiki/Observability) | The `Observer` hook and what each event means |
| [Error Handling](https://github.com/sudhi001/mdsmedia/wiki/Error-Handling) | The error taxonomy and what to retry |
| [Gateway Responses](https://github.com/sudhi001/mdsmedia/wiki/Gateway-Responses) | What the gateway actually returns, and why it is tricky |
| [CLI](https://github.com/sudhi001/mdsmedia/wiki/CLI) | Full command and flag reference |
| [Troubleshooting](https://github.com/sudhi001/mdsmedia/wiki/Troubleshooting) | Symptoms → causes |
| [FAQ](https://github.com/sudhi001/mdsmedia/wiki/FAQ) | Common questions |

## Features

| feature | default | effect |
|---|:---:|---|
| `rustls-tls` | ✅ | TLS via rustls — no OpenSSL to link |
| `native-tls` | | TLS via the platform stack instead |
| `json` | ✅ | parse JSON gateway responses as well as plain text |
| `serde` | | `Serialize`/`Deserialize` on `Config` and `Response` |
| `blocking` | | `blocking::BlockingMdsClient` for sync callers |
| `cli` | | the `mdsmedia` command-line tool |

The library pulls in `reqwest`, `tokio` (time only), `futures-util`, `thiserror`
and `url`. `clap` is CLI-only; `serde`/`serde_json` are optional. No `unsafe`.

## Gateway responses

MDS deployments are inconsistent — the same account can answer with a bare id,
`id|number`, `ERROR: ...`, JSON, or HTML-ish markup. One live endpoint
returns:

```
Message Submitted successfully<pre>msg-id : MESSAGE_ID<pre>Total Invalid Numbers : 0
```

Three things there will break a naive parser, and each is covered by a test:

- **`<pre>` is a field separator**, not markup to render. Tags are flattened to
  newlines before parsing.
- **`Total Invalid Numbers : 0` contains the word "Invalid"** on a *successful*
  send. Counters naming a failure mode are read as numbers: `0` is a clean send,
  non-zero means the recipient was rejected (for single-recipient sends, ours).
  A substring search for "invalid" reports every success as a failure.
- **`msg-id` is alphanumeric**, not a numeric id — labelled ids are taken
  verbatim rather than pattern-matched.

The verbatim body is always kept on `Response::raw`, so nothing is lost when a
deployment does something new.

## Testing

```bash
cargo test --all-features     # 54 tests, incl. mock-gateway integration tests
cargo clippy --all-features --all-targets
```

Integration tests run against an in-process mock gateway, so they need no
network access and no credentials.

## Credentials

No real credentials, phone numbers, endpoints, or DLT registration ids appear
anywhere in this document. Every value is a placeholder — `PHONE_NUMBER`,
`XXXXXX`, `MESSAGE_ID` — and none of them will work as-is. Supply your own via
environment variables or `--env-file`; `*.env` is gitignored.

Note that this gateway takes `apikey` as a **query parameter**, so the key
appears in any URL that gets logged. This crate strips URLs from every error
before it escapes, but be careful with proxy logs, browser history, and shell
history on your own side — prefer `--env-file` over the `--api-key` flag, which
lands in both shell history and `ps` output.

## Author

**Sudhi S** — <support@sudhi.in>
<https://github.com/sudhi001/mdsmedia>

Issues and pull requests are welcome. Since the gateway has no published
specification, reports of response shapes this crate mis-parses are especially
useful — please include the verbatim `Response::raw` body, with any credentials
and phone numbers redacted.

Stuck on something? Check
[Troubleshooting](https://github.com/sudhi001/mdsmedia/wiki/Troubleshooting)
first — it is organised symptom-first, and leads with the case where the API
reports success but no SMS arrives.

## License

MIT © 2026 Sudhi S — see [LICENSE](LICENSE). Provided as-is, without warranty
of any kind.

`mdsmedia` is an independent, unofficial client. MDS Media is not affiliated
with this project, does not endorse it, and provides no support for it. For
questions about this library contact the author above — **not** MDS Media.
