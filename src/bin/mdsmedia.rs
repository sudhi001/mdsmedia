//! `mdsmedia` — command-line tool for testing an MDS Media account.
//!
//! Unofficial; not affiliated with or endorsed by MDS Media.
//! Author: Sudhi S <support@sudhi.in>
//!
//! Build with: `cargo build --release --features cli`
//!
//! Credentials come from `MDS_*` environment variables (or a `--env-file`),
//! and every one of them can be overridden by a flag.

use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use mdsmedia::{
    Config, Event, FailoverClient, MdsClient, Message, Response, RetryPolicy, Route, Template,
};

#[derive(Parser, Debug)]
#[command(
    name = "mdsmedia",
    version,
    author = "Sudhi S <support@sudhi.in>",
    about = "Send and test SMS through the MDS Media gateway (unofficial)",
    long_about = "Send and test SMS through the MDS Media (mdssend.in) gateway.\n\n\
                  UNOFFICIAL: not affiliated with, endorsed by, or supported by\n\
                  MDS Media. Report problems to the author, not to MDS Media.\n\n\
                  Credentials are read from MDS_USERNAME, MDS_API_KEY and\n\
                  MDS_SENDER_ID, each overridable by the matching flag.\n\n\
                  MDS_BASE_URL, MDS_ROUTE, MDS_TEMPLATE_ID, MDS_ENTITY_ID and\n\
                  MDS_COUNTRY_CODE are optional; the endpoint defaults to\n\
                  https://mdssend.in/api.php."
)]
struct Cli {
    #[command(flatten)]
    creds: Creds,

    /// Load KEY=VALUE lines from a file before reading the environment.
    #[arg(long, global = true, value_name = "PATH")]
    env_file: Option<PathBuf>,

    /// Print results as JSON instead of human-readable lines.
    #[arg(long, global = true)]
    json: bool,

    /// Build the request and print it, but do not contact the gateway.
    #[arg(long, global = true)]
    dry_run: bool,

    /// Per-attempt request timeout, in seconds.
    #[arg(long, global = true, default_value_t = 20, value_name = "SECS")]
    timeout: u64,

    /// Retries after the first attempt.
    #[arg(long, global = true, default_value_t = 3, value_name = "N")]
    retries: u32,

    /// Suppress progress output; print only results.
    #[arg(short, long, global = true)]
    quiet: bool,

    /// Print each retry, slow send, and failover hop as it happens.
    #[arg(long, global = true)]
    trace: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Debug, Clone)]
struct Creds {
    /// Gateway endpoint (env: MDS_BASE_URL). Defaults to https://mdssend.in/api.php.
    #[arg(long, global = true, value_name = "URL")]
    base_url: Option<String>,
    /// Fallback endpoint tried if the primary fails (env: MDS_FALLBACK_URL).
    /// Reuses the same credentials, which suits paired accounts that differ
    /// only by URL and DLT template id.
    #[arg(long, global = true, value_name = "URL")]
    fallback_url: Option<String>,
    /// DLT template id for the fallback endpoint (env: MDS_FALLBACK_TEMPLATE_ID).
    #[arg(long, global = true, value_name = "TID")]
    fallback_template_id: Option<String>,
    /// Account username (env: MDS_USERNAME).
    #[arg(long, global = true)]
    username: Option<String>,
    /// Account API key (env: MDS_API_KEY).
    #[arg(long, global = true, value_name = "KEY")]
    api_key: Option<String>,
    /// Approved sender header (env: MDS_SENDER_ID).
    #[arg(long, global = true, value_name = "ID")]
    sender_id: Option<String>,
    /// DLT route: TRANS, PROMO, or a custom value (env: MDS_ROUTE).
    #[arg(long, global = true)]
    route: Option<String>,
    /// DLT template id (env: MDS_TEMPLATE_ID).
    #[arg(long, global = true, value_name = "TID")]
    template_id: Option<String>,
    /// DLT entity id (env: MDS_ENTITY_ID).
    #[arg(long, global = true, value_name = "PEID")]
    entity_id: Option<String>,
    /// Country code for bare local numbers (env: MDS_COUNTRY_CODE).
    #[arg(long, global = true, value_name = "CC")]
    country_code: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Send literal text to one or more numbers.
    Send {
        /// Recipients. Repeat the flag or pass a comma-separated list.
        #[arg(short, long, value_name = "NUMBER", value_delimiter = ',', required = true)]
        to: Vec<String>,
        /// Message body. Use `-` to read the body from stdin.
        #[arg(short = 'm', long, value_name = "TEXT")]
        text: String,
        /// Max sends in flight.
        #[arg(short = 'c', long, default_value_t = 8, value_name = "N")]
        concurrency: usize,
    },

    /// Render a DLT template and send it as an OTP.
    Otp {
        #[arg(short, long, value_name = "NUMBER", value_delimiter = ',', required = true)]
        to: Vec<String>,
        /// The OTP code substituted into the first {#var#}.
        #[arg(short = 'o', long, value_name = "CODE")]
        code: String,
        /// Template text. Defaults to the standard single-{#var#} OTP wording.
        #[arg(
            long,
            value_name = "TEXT",
            default_value = "{#var#} is your OTP. Do not share it with anyone."
        )]
        template: String,
        #[arg(short = 'c', long, default_value_t = 8, value_name = "N")]
        concurrency: usize,
    },

    /// Send one body to every number in a file (one per line).
    Bulk {
        /// File of recipients, one per line. `#` comments and blanks skipped.
        #[arg(short, long, value_name = "PATH")]
        file: PathBuf,
        /// Message body. Use `-` to read the body from stdin.
        #[arg(short = 'm', long, value_name = "TEXT")]
        text: String,
        #[arg(short = 'c', long, default_value_t = 16, value_name = "N")]
        concurrency: usize,
    },

    /// Validate credentials and print the resolved config. Sends nothing.
    Check,

    /// Normalize numbers the way the client would, without sending.
    Normalize {
        #[arg(value_name = "NUMBER", required = true)]
        numbers: Vec<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Some(path) = &cli.env_file {
        if let Err(e) = load_env_file(path) {
            eprintln!("error: reading {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    }

    let config = match resolve_config(&cli.creds) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            eprintln!("hint: set MDS_USERNAME / MDS_API_KEY / MDS_SENDER_ID, or pass the matching flags");
            return ExitCode::FAILURE;
        }
    };

    match run(cli, config) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli, config: Config) -> Result<ExitCode, Box<dyn std::error::Error>> {
    // Commands that never touch the network need no runtime.
    match &cli.command {
        Command::Check => {
            print_check(&config, cli.json);
            return Ok(ExitCode::SUCCESS);
        }
        Command::Normalize { numbers } => {
            return Ok(print_normalize(&config, numbers, cli.json));
        }
        _ => {}
    }

    let fallback = cli
        .creds
        .fallback_url
        .clone()
        .or_else(|| std::env::var("MDS_FALLBACK_URL").ok())
        .map(|url| url.trim().to_string())
        .filter(|u| !u.is_empty());

    let trace = cli.trace;
    let make = |cfg: Config, name: String| -> Result<MdsClient, mdsmedia::Error> {
        let mut builder = MdsClient::builder()
            .config(cfg)
            .name(name)
            .timeout(Duration::from_secs(cli.timeout))
            .retry(RetryPolicy {
                max_retries: cli.retries,
                ..RetryPolicy::default()
            });
        if trace {
            builder = builder.observer(trace_observer);
        }
        builder.build()
    };

    let primary_name = if fallback.is_some() { "primary" } else { "mds" };
    let client = make(config.clone(), primary_name.to_string())?;

    let chain = match &fallback {
        Some(url) => {
            let fallback_cfg = Config {
                base_url: url.clone(),
                template_id: cli
                    .creds
                    .fallback_template_id
                    .clone()
                    .or_else(|| std::env::var("MDS_FALLBACK_TEMPLATE_ID").ok())
                    .or_else(|| config.template_id.clone()),
                ..config.clone()
            };
            let secondary = make(fallback_cfg, "fallback".to_string())?;
            let mut fc = FailoverClient::new(vec![client.clone(), secondary])?;
            if trace {
                fc = fc.observer(trace_observer);
            }
            Some(fc)
        }
        None => None,
    };

    let (messages, concurrency) = match &cli.command {
        Command::Send { to, text, concurrency } => {
            let body = read_text(text)?;
            (
                to.iter().map(|n| Message::new(n.trim(), body.clone())).collect::<Vec<_>>(),
                *concurrency,
            )
        }
        Command::Otp { to, code, template, concurrency } => {
            let tpl = Template::new(template.clone());
            let body = tpl.render_one(code);
            (
                to.iter().map(|n| Message::new(n.trim(), body.clone())).collect(),
                *concurrency,
            )
        }
        Command::Bulk { file, text, concurrency } => {
            let body = read_text(text)?;
            let numbers = read_number_file(file)?;
            if numbers.is_empty() {
                return Err(format!("no recipients found in {}", file.display()).into());
            }
            (
                numbers.iter().map(|n| Message::new(n, body.clone())).collect(),
                *concurrency,
            )
        }
        Command::Check | Command::Normalize { .. } => unreachable!("handled above"),
    };

    if cli.dry_run {
        print_dry_run(client.config(), &messages, cli.json);
        if let (Some(url), false) = (&fallback, cli.json) {
            println!("fallback: {url}");
        }
        return Ok(ExitCode::SUCCESS);
    }

    if !cli.quiet && !cli.json {
        match &chain {
            Some(fc) => eprintln!(
                "sending {} message(s) at concurrency {} via {}...",
                messages.len(),
                concurrency,
                fc.provider_names().join(" -> ")
            ),
            None => eprintln!(
                "sending {} message(s) at concurrency {}...",
                messages.len(),
                concurrency
            ),
        }
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let results = runtime.block_on(async {
        match &chain {
            Some(fc) => fc.send_many_with_concurrency(messages, concurrency).await,
            None => client.send_many_with_concurrency(messages, concurrency).await,
        }
    });

    let failures = report(&results, cli.json);
    Ok(if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Flags win over env vars; env vars fill the rest.
fn resolve_config(creds: &Creds) -> Result<Config, mdsmedia::Error> {
    fn pick(flag: &Option<String>, key: &str) -> Option<String> {
        flag.clone()
            .or_else(|| std::env::var(key).ok())
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }

    let config = Config {
        base_url: pick(&creds.base_url, "MDS_BASE_URL")
            .unwrap_or_else(|| mdsmedia::DEFAULT_BASE_URL.to_string()),
        username: pick(&creds.username, "MDS_USERNAME")
            .ok_or(mdsmedia::Error::MissingConfig("username"))?,
        api_key: pick(&creds.api_key, "MDS_API_KEY")
            .ok_or(mdsmedia::Error::MissingConfig("api_key"))?,
        sender_id: pick(&creds.sender_id, "MDS_SENDER_ID")
            .ok_or(mdsmedia::Error::MissingConfig("sender_id"))?,
        route: pick(&creds.route, "MDS_ROUTE")
            .map(|r| Route::from(r.as_str()))
            .unwrap_or_default(),
        template_id: pick(&creds.template_id, "MDS_TEMPLATE_ID"),
        entity_id: pick(&creds.entity_id, "MDS_ENTITY_ID"),
        default_country_code: pick(&creds.country_code, "MDS_COUNTRY_CODE"),
    };
    Ok(config)
}

/// Minimal `.env` reader: `KEY=VALUE`, `#` comments, optional quotes.
/// Existing environment variables win, so an exported secret is not clobbered.
fn load_env_file(path: &PathBuf) -> std::io::Result<()> {
    let content = std::fs::read_to_string(path)?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
        if std::env::var_os(key).is_none() {
            std::env::set_var(key, value);
        }
    }
    Ok(())
}

/// `-` means "read the body from stdin", so bodies with quotes or newlines can
/// be piped in without shell escaping.
fn read_text(arg: &str) -> Result<String, Box<dyn std::error::Error>> {
    if arg != "-" {
        return Ok(arg.to_string());
    }
    if std::io::stdin().is_terminal() {
        return Err("`--text -` expects the message body on stdin".into());
    }
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    let buf = buf.trim_end_matches('\n').to_string();
    if buf.is_empty() {
        return Err("stdin was empty".into());
    }
    Ok(buf)
}

fn read_number_file(path: &PathBuf) -> std::io::Result<Vec<String>> {
    Ok(std::fs::read_to_string(path)?
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

fn print_check(config: &Config, json: bool) {
    let route = config.route.to_string();
    let endpoint = config.effective_base_url();
    let is_default = endpoint == mdsmedia::DEFAULT_BASE_URL;
    if json {
        println!(
            "{{\"ok\":true,\"base_url\":{},\"base_url_is_default\":{},\"username\":{},\"sender_id\":{},\"route\":{},\"template_id\":{},\"entity_id\":{},\"country_code\":{},\"api_key\":{}}}",
            q(endpoint),
            is_default,
            q(&config.username),
            q(&config.sender_id),
            q(&route),
            opt_q(config.template_id.as_deref()),
            opt_q(config.entity_id.as_deref()),
            opt_q(config.default_country_code.as_deref()),
            q(&mask(&config.api_key)),
        );
    } else {
        println!("configuration OK");
        println!(
            "  base_url     {endpoint}{}",
            if is_default { "  (default)" } else { "" }
        );
        println!("  username     {}", config.username);
        println!("  api_key      {}", mask(&config.api_key));
        println!("  sender_id    {}", config.sender_id);
        println!("  route        {route}");
        println!("  template_id  {}", config.template_id.as_deref().unwrap_or("-"));
        println!("  entity_id    {}", config.entity_id.as_deref().unwrap_or("-"));
        println!(
            "  country_code {}",
            config.default_country_code.as_deref().unwrap_or("-")
        );
    }
}

fn print_normalize(config: &Config, numbers: &[String], json: bool) -> ExitCode {
    // Normalization runs through a built client so the CLI exercises exactly
    // the same path a real send would.
    let client = match MdsClient::from_config(config.clone()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut bad = 0;
    for n in numbers {
        match client.normalize(n) {
            Ok(norm) => {
                if json {
                    println!("{{\"input\":{},\"normalized\":{}}}", q(n), q(&norm));
                } else {
                    println!("{n}\t-> {norm}");
                }
            }
            Err(e) => {
                bad += 1;
                if json {
                    println!("{{\"input\":{},\"error\":{}}}", q(n), q(&e.to_string()));
                } else {
                    println!("{n}\t-> ERROR: {e}");
                }
            }
        }
    }
    if bad == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn print_dry_run(config: &Config, messages: &[Message], json: bool) {
    if !json {
        println!("endpoint: {}", config.effective_base_url());
    }
    for m in messages {
        let normalized = config
            .normalize(m.recipient())
            .unwrap_or_else(|e| format!("INVALID ({e})"));
        if json {
            println!(
                "{{\"dry_run\":true,\"to\":{},\"normalized\":{},\"body\":{},\"len\":{}}}",
                q(m.recipient()),
                q(&normalized),
                q(m.body()),
                m.body().len()
            );
        } else {
            println!("DRY RUN  {} -> {normalized}", m.recipient());
            println!("  body ({} bytes): {}", m.body().len(), m.body());
        }
    }
}

/// Prints one line per result and returns the failure count.
fn report(results: &[(String, mdsmedia::Result<Response>)], json: bool) -> usize {
    let mut failures = 0;
    for (to, result) in results {
        match result {
            Ok(r) => {
                if json {
                    println!(
                        "{{\"ok\":true,\"to\":{},\"provider\":{},\"message_id\":{},\"status\":{},\"attempts\":{},\"elapsed_ms\":{},\"raw\":{}}}",
                        q(&r.to),
                        q(&r.provider),
                        opt_q(r.message_id.as_deref()),
                        r.status,
                        r.attempts,
                        r.elapsed.as_millis(),
                        q(&r.raw)
                    );
                } else {
                    println!(
                        "OK    {}  via={}  id={}  {}ms  attempt(s)={}  raw={}",
                        r.to,
                        r.provider,
                        r.message_id.as_deref().unwrap_or("-"),
                        r.elapsed.as_millis(),
                        r.attempts,
                        r.raw.trim()
                    );
                }
            }
            Err(e) => {
                failures += 1;
                if json {
                    println!(
                        "{{\"ok\":false,\"to\":{},\"error\":{},\"retryable\":{},\"fatal\":{}}}",
                        q(to),
                        q(&e.to_string()),
                        e.is_retryable(),
                        e.is_fatal()
                    );
                } else {
                    println!("FAIL  {to}  {e}");
                }
            }
        }
    }
    if !json && !results.is_empty() {
        eprintln!("{}/{} sent", results.len() - failures, results.len());
    }
    failures
}

/// Prints the send lifecycle to stderr, so `--trace` shows retries and
/// failover hops without polluting the parseable stdout stream.
fn trace_observer(event: Event<'_>) {
    match event {
        Event::Attempt {
            provider,
            to,
            attempt,
        } => eprintln!("  [{provider}] attempt {attempt} -> {to}"),
        Event::Retrying {
            provider,
            attempt,
            delay,
            error,
            ..
        } => eprintln!("  [{provider}] attempt {attempt} failed ({error}); retrying in {delay:?}"),
        Event::Slow {
            provider,
            to,
            elapsed,
        } => eprintln!("  [{provider}] SLOW {to} took {elapsed:?}"),
        Event::Failed {
            provider,
            to,
            error,
        } => eprintln!("  [{provider}] gave up on {to}: {error}"),
        Event::FailingOver {
            from,
            to_provider,
            error,
            ..
        } => eprintln!("  [{from}] -> [{to_provider}]: {error}"),
        _ => {}
    }
}

/// Shows enough of the key to tell two accounts apart, without printing it.
fn mask(secret: &str) -> String {
    let n = secret.chars().count();
    if n <= 4 {
        return "*".repeat(n);
    }
    let tail: String = secret.chars().skip(n - 4).collect();
    format!("{}{}", "*".repeat(n - 4), tail)
}

/// Minimal JSON string escaping — the CLI emits JSON without pulling in serde.
fn q(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn opt_q(s: Option<&str>) -> String {
    match s {
        Some(v) => q(v),
        None => "null".to_string(),
    }
}
