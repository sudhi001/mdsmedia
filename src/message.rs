//! Message bodies and DLT templates.

/// The DLT placeholder token used in approved Indian SMS templates.
pub const VAR: &str = "{#var#}";

/// Maximum body length accepted before the client refuses to send.
/// (10 GSM-7 concatenated parts; the gateway itself bills per 153-char part.)
pub const MAX_BODY_LEN: usize = 1530;

/// A DLT-approved message template with `{#var#}` placeholders.
///
/// ```
/// use mdsmedia::Template;
/// let t = Template::new("{#var#} is your OTP. Valid for {#var#} minutes.");
/// assert_eq!(t.placeholders(), 2);
/// assert_eq!(t.render(&["OTP_CODE", "5"]), "OTP_CODE is your OTP. Valid for 5 minutes.");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    raw: String,
}

impl Template {
    pub fn new(raw: impl Into<String>) -> Self {
        Template { raw: raw.into() }
    }

    /// The template text, placeholders unexpanded.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// How many `{#var#}` placeholders the template contains.
    pub fn placeholders(&self) -> usize {
        self.raw.matches(VAR).count()
    }

    /// Substitutes `vars` into the placeholders, left to right.
    ///
    /// Extra placeholders are left as-is and extra vars are ignored, rather
    /// than erroring — a partially-filled body is easier to spot in gateway
    /// logs than a send that silently never happened.
    pub fn render<S: AsRef<str>>(&self, vars: &[S]) -> String {
        let mut out = self.raw.clone();
        for v in vars {
            match out.find(VAR) {
                Some(idx) => out.replace_range(idx..idx + VAR.len(), v.as_ref()),
                None => break,
            }
        }
        out
    }

    /// Renders with a single variable — the common OTP case.
    pub fn render_one(&self, var: impl AsRef<str>) -> String {
        self.render(&[var.as_ref()])
    }
}

impl From<&str> for Template {
    fn from(s: &str) -> Self {
        Template::new(s)
    }
}

impl From<String> for Template {
    fn from(s: String) -> Self {
        Template::new(s)
    }
}

/// One outbound SMS: a recipient plus a body, with optional per-message
/// overrides of the account defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub(crate) to: String,
    pub(crate) body: String,
    pub(crate) sender_id: Option<String>,
    pub(crate) route: Option<crate::Route>,
    pub(crate) template_id: Option<String>,
    /// Opaque caller-side tag, echoed back on the response. Useful for
    /// correlating results out of [`crate::MdsClient::send_many`], which
    /// completes out of order.
    pub(crate) reference: Option<String>,
}

impl Message {
    /// A message to `to` with the literal text `body`.
    pub fn new(to: impl Into<String>, body: impl Into<String>) -> Self {
        Message {
            to: to.into(),
            body: body.into(),
            sender_id: None,
            route: None,
            template_id: None,
            reference: None,
        }
    }

    /// A message rendered from a DLT template.
    pub fn from_template<S: AsRef<str>>(
        to: impl Into<String>,
        template: &Template,
        vars: &[S],
    ) -> Self {
        Message::new(to, template.render(vars))
    }

    /// Override the account sender id for this message only.
    pub fn sender_id(mut self, sender: impl Into<String>) -> Self {
        self.sender_id = Some(sender.into());
        self
    }

    /// Override the account route for this message only.
    pub fn route(mut self, route: impl Into<crate::Route>) -> Self {
        self.route = Some(route.into());
        self
    }

    /// Override the account DLT template id for this message only.
    pub fn template_id(mut self, tid: impl Into<String>) -> Self {
        self.template_id = Some(tid.into());
        self
    }

    /// Attach a correlation tag echoed back on [`crate::Response::reference`].
    pub fn reference(mut self, reference: impl Into<String>) -> Self {
        self.reference = Some(reference.into());
        self
    }

    pub fn recipient(&self) -> &str {
        &self.to
    }

    pub fn body(&self) -> &str {
        &self.body
    }

    pub(crate) fn validate(&self) -> crate::Result<()> {
        if self.body.trim().is_empty() {
            return Err(crate::Error::InvalidMessage("body is empty"));
        }
        if self.body.len() > MAX_BODY_LEN {
            return Err(crate::Error::InvalidMessage(
                "body exceeds the maximum of 1530 bytes",
            ));
        }
        Ok(())
    }
}
