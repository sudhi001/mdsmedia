//! Gateway response parsing.
//!
//! MDS deployments are not consistent: the same account can answer with a bare
//! message id, a `id|number` pair, a `ERROR: ...` line, or JSON. Parsing is
//! therefore layered and always keeps [`Response::raw`] so callers can inspect
//! whatever the gateway actually said.

use std::time::Duration;

/// A successful submission to the gateway.
///
/// "Accepted" means the gateway took the message — not that the handset
/// received it. MDS exposes final delivery only through DLR callbacks.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Response {
    /// Name of the provider that accepted the message. With a
    /// [`crate::FailoverClient`] this tells you which endpoint actually served
    /// the send — the whole point of having a fallback.
    pub provider: String,
    /// Gateway-assigned message id, when one could be extracted.
    pub message_id: Option<String>,
    /// The normalized recipient the message was submitted for.
    pub to: String,
    /// HTTP status returned by the gateway.
    pub status: u16,
    /// Verbatim response body.
    pub raw: String,
    /// Wall-clock time of the submission, including retries.
    pub elapsed: Duration,
    /// Number of attempts made (1 = succeeded first try).
    pub attempts: u32,
    /// The tag set via [`crate::Message::reference`], echoed back.
    pub reference: Option<String>,
}

impl Response {
    /// True when the gateway returned an id we could parse.
    pub fn has_message_id(&self) -> bool {
        self.message_id.is_some()
    }
}

/// Outcome of parsing a 2xx body: either accepted, or an explicit rejection.
pub(crate) enum Parsed {
    Accepted { message_id: Option<String> },
    Rejected { reason: String },
}

/// Tokens that mark a 2xx body as a rejection. MDS returns HTTP 200 even for
/// credential and template failures, so the body is the only signal.
const FAILURE_TOKENS: [&str; 8] = [
    "error",
    "invalid",
    "failed",
    "failure",
    "reject",
    "insufficient",
    "unauthor",
    "not allowed",
];

/// Tokens that positively confirm the gateway accepted the message. These win
/// over an incidental failure word elsewhere in the body — a real response
/// reads `Message Submitted successfully<pre>msg-id : ...<pre>Total Invalid
/// Numbers : 0`, where "Invalid" belongs to a zero-valued counter.
const SUCCESS_TOKENS: [&str; 5] = [
    "submitted successfully",
    "sent successfully",
    "success",
    "msg-id",
    "message id",
];

/// Normalized `key : value` labels that carry a message id.
const ID_LABELS: [&str; 6] = ["msgid", "messageid", "id", "smsid", "jobid", "batchid"];

pub(crate) fn parse_body(body: &str) -> Parsed {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Parsed::Rejected {
            reason: "gateway returned an empty body".to_string(),
        };
    }

    #[cfg(feature = "json")]
    if let Some(parsed) = parse_json(trimmed) {
        return parsed;
    }

    let flat = flatten(trimmed);
    let mut saw_success = false;
    let mut token_failure: Option<String> = None;

    for segment in flat
        .split(['\n', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let lower = segment.to_ascii_lowercase();

        // A counter naming a failure mode is authoritative: zero means the
        // send was clean, non-zero means our recipient was the bad one.
        if let Some(count) = failure_counter(&lower) {
            if count > 0 {
                return Parsed::Rejected {
                    reason: segment.to_string(),
                };
            }
            continue;
        }

        if SUCCESS_TOKENS.iter().any(|t| lower.contains(t)) {
            saw_success = true;
            continue;
        }

        if token_failure.is_none() {
            if let Some(token) = FAILURE_TOKENS.iter().find(|t| lower.contains(**t)) {
                if is_standalone(&lower, token) {
                    token_failure = Some(segment.to_string());
                }
            }
        }
    }

    if !saw_success {
        if let Some(reason) = token_failure {
            return Parsed::Rejected { reason };
        }
    }

    Parsed::Accepted {
        message_id: extract_id(&flat),
    }
}

/// Flattens HTML-ish markup to newlines. Some deployments separate status
/// fields with `<pre>` / `<br>` rather than newlines or pipes.
fn flatten(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut in_tag = false;
    for c in body.chars() {
        match c {
            '<' => {
                in_tag = true;
                out.push('\n');
            }
            '>' if in_tag => {
                in_tag = false;
                out.push('\n');
            }
            _ if in_tag => {}
            _ => out.push(c),
        }
    }
    out
}

/// Splits `label : value`, returning the normalized label and trimmed value.
fn split_labeled(segment: &str) -> Option<(String, &str)> {
    let (label, value) = segment.split_once(':')?;
    let normalized: String = label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect();
    Some((normalized, value.trim()))
}

/// For a segment like `Total Invalid Numbers : 0`, returns the count. `None`
/// when the segment is not a numeric counter naming a failure mode.
fn failure_counter(lower_segment: &str) -> Option<u64> {
    let (label, value) = split_labeled(lower_segment)?;
    if ID_LABELS.contains(&label.as_str()) {
        return None;
    }
    if !["invalid", "fail", "error", "reject", "duplicate"]
        .iter()
        .any(|t| label.contains(t))
    {
        return None;
    }
    value.parse::<u64>().ok()
}

/// True when `token` in `haystack` is bounded by non-alphanumeric characters,
/// so `ERROR: bad key` matches but an id like `x7error9` does not.
fn is_standalone(haystack: &str, token: &str) -> bool {
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(token) {
        let start = from + rel;
        let end = start + token.len();
        let before_ok = start == 0
            || !haystack[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric());
        let after_ok = end == haystack.len()
            || !haystack[end..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

/// Pulls a message id out of the common shapes: `msg-id : Xy7Kq2mBn4Rv8Ts`,
/// `1234567`, `1234567|9995551212`, `Sent: 1234567`, `id=1234567`.
fn extract_id(body: &str) -> Option<String> {
    // An explicitly labelled id wins, and is taken verbatim — gateway ids are
    // not always numeric or long.
    for segment in body.split(['\n', ';']).map(str::trim) {
        if let Some((label, value)) = split_labeled(segment) {
            if ID_LABELS.contains(&label.as_str())
                && !value.is_empty()
                && !value.contains(char::is_whitespace)
            {
                return Some(value.to_string());
            }
        }
    }

    let first_line = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or(body);

    for sep in ['|', ','] {
        if let Some((head, _)) = first_line.split_once(sep) {
            let head = head.trim();
            if is_idish(head) {
                return Some(head.to_string());
            }
        }
    }

    if let Some((_, tail)) = first_line.split_once('=') {
        let tail = tail.trim();
        if is_idish(tail) {
            return Some(tail.to_string());
        }
    }

    if is_idish(first_line) {
        return Some(first_line.to_string());
    }

    // Fall back to the longest id-looking whitespace-separated token.
    first_line
        .split_whitespace()
        .filter(|t| is_idish(t))
        .max_by_key(|t| t.len())
        .map(|t| t.to_string())
}

/// An id is 6+ characters of alphanumerics/hyphens containing at least one digit.
fn is_idish(s: &str) -> bool {
    let s = s.trim_matches(|c: char| c == ':' || c == '.' || c == '"');
    s.len() >= 6
        && s.chars().any(|c| c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(feature = "json")]
fn parse_json(body: &str) -> Option<Parsed> {
    if !body.starts_with('{') && !body.starts_with('[') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    // Arrays come back as one entry per recipient; inspect the first.
    let obj = match &value {
        serde_json::Value::Array(items) => items.first()?,
        other => other,
    };

    let get = |keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|k| match obj.get(k)? {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
    };

    let status = get(&["status", "Status", "type", "result"]).unwrap_or_default();
    let status_lower = status.to_ascii_lowercase();
    let is_failure = FAILURE_TOKENS.iter().any(|t| status_lower.contains(t))
        || matches!(obj.get("success"), Some(serde_json::Value::Bool(false)));

    if is_failure {
        let reason = get(&["message", "Message", "description", "error", "reason"])
            .unwrap_or_else(|| body.to_string());
        return Some(Parsed::Rejected { reason });
    }

    Some(Parsed::Accepted {
        message_id: get(&["message_id", "messageId", "msgid", "msgId", "id", "MessageId"]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id_of(body: &str) -> Option<String> {
        match parse_body(body) {
            Parsed::Accepted { message_id } => message_id,
            Parsed::Rejected { reason } => panic!("unexpected rejection: {reason}"),
        }
    }

    fn is_rejected(body: &str) -> bool {
        matches!(parse_body(body), Parsed::Rejected { .. })
    }

    #[test]
    fn plain_id_shapes() {
        assert_eq!(id_of("1234567890123456789"), Some("1234567890123456789".into()));
        assert_eq!(id_of("8812345|919995551212"), Some("8812345".into()));
        assert_eq!(id_of("id=8812345"), Some("8812345".into()));
        assert_eq!(id_of("Message Submitted 8812345"), Some("8812345".into()));
    }

    #[test]
    fn success_without_id_is_still_accepted() {
        assert_eq!(id_of("Submitted Successfully"), None);
    }

    /// A body observed from a live MDS deployment on a send that was confirmed
    /// delivered to the handset. The literal word "Invalid" appears in a
    /// zero-valued counter and must not read as failure.
    #[test]
    fn live_gateway_success_response() {
        let body = "Message Submitted successfully<pre>msg-id : Xy7Kq2mBn4Rv8Ts<pre>Total Invalid Numbers : 0";
        assert_eq!(id_of(body), Some("Xy7Kq2mBn4Rv8Ts".into()));
    }

    #[test]
    fn a_nonzero_invalid_counter_is_a_rejection() {
        // Single-recipient sends: one invalid number means ours was rejected.
        assert!(is_rejected(
            "Message Submitted successfully<pre>msg-id : abc123def<pre>Total Invalid Numbers : 1"
        ));
    }

    #[test]
    fn html_separators_are_flattened() {
        assert_eq!(id_of("Sent<br>msg-id : XY12345<br>"), Some("XY12345".into()));
    }

    #[test]
    fn failure_bodies_are_rejections() {
        assert!(is_rejected("ERROR: Invalid API Key"));
        assert!(is_rejected("Authentication Failed"));
        assert!(is_rejected("Insufficient Credits"));
        assert!(is_rejected(""));
    }

    #[test]
    fn id_containing_a_failure_token_is_not_a_rejection() {
        assert_eq!(id_of("ab12errorid99"), Some("ab12errorid99".into()));
    }

    #[cfg(feature = "json")]
    #[test]
    fn json_shapes() {
        assert_eq!(
            id_of(r#"{"status":"success","message_id":"8812345"}"#),
            Some("8812345".into())
        );
        assert!(is_rejected(r#"{"status":"error","message":"Invalid Sender"}"#));
        assert!(is_rejected(r#"{"success":false,"message":"blocked"}"#));
    }
}
