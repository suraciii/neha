//! Rules shared by every diagnostic signal.
//!
//! Structured events may quote an error only through [`scrub_error`], and a
//! value a caller supplies may enter an event only through
//! [`sanitize_value`]. The rules keep credentials and content out of logs even
//! when an error message or header is hostile.
//!
//! `eng/diagnostics.md` states the field allowlist and the event catalog; this
//! module is the one implementation of the text rules.

/// The longest error text an event may carry, in characters.
pub const MAX_ERROR_CHARS: usize = 1024;

/// The longest value a user-controlled field may carry, in characters.
pub const MAX_FIELD_CHARS: usize = 256;

/// The environment variables whose content never enters a diagnostic signal.
const SECRET_KEYS: [&str; 14] = [
    "access_token",
    "api_key",
    "apikey",
    "authorization",
    "client_secret",
    "cookie",
    "credential",
    "password",
    "passwd",
    "private_key",
    "refresh_token",
    "secret",
    "set-cookie",
    "token",
];

/// Makes an error message safe to put in an event.
///
/// Control characters become spaces, URLs lose their user information, query,
/// and fragment, and a value after a credential-like key is replaced. The
/// result is at most [`MAX_ERROR_CHARS`] characters.
/// Installs the W3C trace-context propagator.
///
/// It is a process-wide setting, so a library start and the binary's
/// diagnostics initialization both call it; the last call wins with the same
/// value.
pub fn install_propagator() {
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );
}

/// Returns a fresh request identifier.
#[must_use]
pub fn request_id() -> String {
    neha::id::Id::new().to_string()
}

/// Extracts the W3C trace context an inbound request carries.
#[must_use]
pub fn inbound_parent(headers: &axum::http::HeaderMap) -> opentelemetry::Context {
    use opentelemetry::propagation::Extractor;

    struct Headers<'a>(&'a axum::http::HeaderMap);
    impl Extractor for Headers<'_> {
        fn get(&self, key: &str) -> Option<&str> {
            self.0.get(key).and_then(|value| value.to_str().ok())
        }
        fn keys(&self) -> Vec<&str> {
            self.0.keys().map(axum::http::HeaderName::as_str).collect()
        }
    }

    opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.extract(&Headers(headers))
    })
}

/// Makes the current tracing span a child of `parent`.
pub fn set_parent(span: &tracing::Span, parent: opentelemetry::Context) {
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    let _ = span.set_parent(parent);
}

/// Injects the current span's W3C context into outbound headers.
pub fn inject_headers(headers: &mut reqwest::header::HeaderMap) {
    // Only the agreed field crosses the boundary: a vendor `tracestate` is
    // not forwarded.
    let Some(traceparent) = current_traceparent() else {
        return;
    };
    if let Ok(value) = reqwest::header::HeaderValue::from_str(&traceparent) {
        headers.insert(
            reqwest::header::HeaderName::from_static("traceparent"),
            value,
        );
    }
}

/// Adds the current W3C context to JSON-RPC `params._meta`.
pub fn inject_json_meta(params: &mut serde_json::Value) {
    let Some(traceparent) = current_traceparent() else {
        return;
    };
    let Some(object) = params.as_object_mut() else {
        return;
    };
    let meta = object
        .entry("_meta")
        .or_insert_with(|| serde_json::json!({}));
    if let Some(meta) = meta.as_object_mut() {
        meta.insert("traceparent".to_owned(), serde_json::json!(traceparent));
    }
}

/// Returns the W3C `traceparent` an outbound frame carries, when the current
/// span has a valid trace.
#[must_use]
pub fn current_traceparent() -> Option<String> {
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    format_traceparent(tracing::Span::current().context().span().span_context())
}

/// Formats one span context as a W3C `traceparent` header value.
#[must_use]
pub fn traceparent(span: &tracing::Span) -> Option<String> {
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    format_traceparent(span.context().span().span_context())
}

/// Returns a W3C `traceparent` for a response even when no subscriber is
/// installed: the inbound trace id is kept, and otherwise a new trace starts.
#[must_use]
pub fn response_traceparent(span: &tracing::Span, inbound: &opentelemetry::Context) -> String {
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    if let Some(traceparent) = format_traceparent(span.context().span().span_context()) {
        return traceparent;
    }
    let inbound = inbound.span().span_context().clone();
    if inbound.is_valid() {
        return format!(
            "00-{:032x}-{:016x}-{:02x}",
            inbound.trace_id(),
            new_span_id(),
            u8::from(inbound.trace_flags().is_sampled())
        );
    }
    format!(
        "00-{:032x}-{:016x}-01",
        rand::random::<u128>(),
        new_span_id()
    )
}

fn new_span_id() -> u64 {
    // A span id is eight bytes and must not be zero.
    loop {
        let id = rand::random::<u64>();
        if id != 0 {
            return id;
        }
    }
}

fn format_traceparent(context: &opentelemetry::trace::SpanContext) -> Option<String> {
    if !context.is_valid() {
        return None;
    }
    let trace = context.trace_id();
    let span = context.span_id();
    let flags = u8::from(context.trace_flags().is_sampled());
    Some(format!("00-{trace:032x}-{span:016x}-{flags:02x}"))
}

#[must_use]
pub fn scrub_error(message: &str) -> String {
    let mut scrubbed = String::with_capacity(message.len());
    for character in message.chars() {
        if character.is_control() {
            scrubbed.push(' ');
        } else {
            scrubbed.push(character);
        }
    }

    let with_quotes = redact_quoted_values(&scrubbed);
    let with_paths_and_urls = scrub_tokens(&with_quotes);
    let with_secrets = redact_secret_values(&with_paths_and_urls);

    truncate(&with_secrets, MAX_ERROR_CHARS)
}

/// Makes a user-controlled value safe to put in an event field.
///
/// It removes control characters, collapses surrounding whitespace, and
/// bounds the length. A request identifier, host header, and similar value
/// stays readable while it cannot inject a line or an unbounded field.
#[must_use]
pub fn sanitize_value(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_control() {
            sanitized.push(' ');
        } else {
            sanitized.push(character);
        }
    }

    truncate(sanitized.trim(), MAX_FIELD_CHARS)
}

/// Names the kind of storage a Space uses, for logs and support reports.
#[must_use]
pub fn storage_kind(storage: &crate::config::Storage) -> &'static str {
    match storage {
        crate::config::Storage::Local => "local",
        crate::config::Storage::S3(_) => "s3",
    }
}

/// Scrubs URLs and absolute paths in a message.
///
/// A path can contain spaces, so a path start keeps removing the following
/// whitespace-delimited pieces until a piece ends with punctuation. The error
/// after the path stays readable.
fn scrub_tokens(message: &str) -> String {
    let mut scrubbed = Vec::new();
    let mut in_path = false;
    for token in message.split_whitespace() {
        if token.contains("://") {
            scrubbed.push(scrub_url_token(token));
            in_path = false;
            continue;
        }
        if in_path || looks_like_path(token) {
            scrubbed.push("[redacted]".to_owned());
            in_path = !ends_path(token);
            continue;
        }
        scrubbed.push(token.to_owned());
    }

    scrubbed.join(" ")
}

fn ends_path(token: &str) -> bool {
    token.chars().next_back().is_some_and(|character| {
        matches!(
            character,
            ':' | ',' | ';' | '.' | ')' | ']' | '}' | '"' | '\''
        )
    })
}

/// Whether a token names an absolute path.
///
/// A local path can hold a user name, a Workspace name, or a file name; the
/// allowlist keeps paths out of diagnostics. Relative words such as `and/or`
/// stay readable.
fn looks_like_path(token: &str) -> bool {
    let token = token.trim_matches(|character: char| {
        matches!(character, ',' | ';' | ')' | ']' | '}' | '"' | '\'')
    });
    token.starts_with('/')
        || token.starts_with("~/")
        || token.starts_with("./")
        || (token.as_bytes().get(1) == Some(&b':')
            && matches!(token.as_bytes().get(2), Some(b'\\' | b'/')))
        // A path with a space arrives as several tokens; a relative piece such
        // as `Workspace/cache.json` still names a private file.
        || (token.contains('/') && (token.contains('.') || token.matches('/').count() >= 2))
}

/// Replaces the user information, query, and fragment of an absolute URL.
///
/// The function works on one whitespace-delimited token so an error that
/// quotes several URLs loses the sensitive part of each without losing the
/// surrounding message.
fn scrub_url_token(token: &str) -> String {
    let Some(scheme_end) = token.find("://") else {
        return token.to_owned();
    };

    let authority_start = scheme_end + 3;
    let authority_end = token[authority_start..]
        .find(['/', '?', '#'])
        .map_or(token.len(), |offset| authority_start + offset);
    let mut authority = &token[authority_start..authority_end];
    if let Some(userinfo_end) = authority.rfind('@') {
        authority = &authority[userinfo_end + 1..];
    }

    let mut scrubbed = String::with_capacity(token.len());
    scrubbed.push_str(&token[..scheme_end + 3]);
    scrubbed.push_str(authority);

    if authority_end < token.len() {
        // A path, query, or fragment can name a document, carry a token, or
        // echo a remote path; keep the origin and drop the rest.
        scrubbed.push_str("/[redacted]");
    }

    scrubbed
}

/// Replaces the value that follows a credential-like key in a message.
fn redact_secret_values(message: &str) -> String {
    let mut redacted = message.to_owned();
    for key in SECRET_KEYS {
        redacted = redact_key(&redacted, key);
    }

    redact_key(&redacted, "bearer")
}

fn redact_key(message: &str, key: &str) -> String {
    let mut result = String::with_capacity(message.len());
    let mut rest = message;
    let mut lower = rest.to_ascii_lowercase();

    loop {
        let Some(position) = lower.find(key) else {
            result.push_str(rest);
            return result;
        };

        let after_key = position + key.len();
        let boundary_before = position == 0
            || lower[..position]
                .chars()
                .next_back()
                .is_none_or(|character| !character.is_ascii_alphanumeric() && character != '_');
        let after = &lower[after_key..];
        let boundary_after = after
            .chars()
            .next()
            .is_none_or(|character| !character.is_ascii_alphanumeric() && character != '_');
        if !boundary_before || !boundary_after {
            result.push_str(&rest[..after_key]);
            rest = &rest[after_key..];
            lower = rest.to_ascii_lowercase();
            continue;
        }

        result.push_str(&rest[..after_key]);
        let mut value_start = after_key;
        let bytes = lower.as_bytes();
        while value_start < bytes.len() && bytes[value_start].is_ascii_whitespace() {
            value_start += 1;
        }
        // A JSON key is written `"password":"value"`; its closing quote sits
        // between the key and the separator.
        while value_start < bytes.len() && matches!(bytes[value_start], b'"' | b'\'') {
            value_start += 1;
            while value_start < bytes.len() && bytes[value_start].is_ascii_whitespace() {
                value_start += 1;
            }
        }
        if value_start < bytes.len() && matches!(bytes[value_start], b'=' | b':') {
            value_start += 1;
            while value_start < bytes.len() && bytes[value_start].is_ascii_whitespace() {
                value_start += 1;
            }
        }
        let redact_start = value_start;

        let value_end = if value_start < bytes.len() && matches!(bytes[value_start], b'"' | b'\'') {
            quoted_end(&lower, value_start).0
        } else {
            let mut credential_start = value_start;
            if key == "authorization" {
                // `Authorization: Bearer <token>` and `Basic <token>` name
                // the scheme before the credential; redact both.
                let word_end = lower[value_start..]
                    .find(char::is_whitespace)
                    .map_or(lower.len(), |offset| value_start + offset);
                let word = &lower[value_start..word_end];
                if word.eq_ignore_ascii_case("bearer") || word.eq_ignore_ascii_case("basic") {
                    credential_start = word_end;
                    while credential_start < bytes.len()
                        && bytes[credential_start].is_ascii_whitespace()
                    {
                        credential_start += 1;
                    }
                }
            }
            let header_value = matches!(key, "authorization" | "cookie" | "set-cookie");
            if header_value {
                lower.len()
            } else {
                lower[credential_start..]
                    .find(|character: char| {
                        character.is_ascii_whitespace()
                            || matches!(character, ',' | ';' | ')' | ']' | '}')
                    })
                    .map_or(lower.len(), |offset| credential_start + offset)
            }
        };

        if value_end == value_start {
            rest = &rest[after_key..];
            lower = rest.to_ascii_lowercase();
            result.push(' ');
            continue;
        }

        result.push_str(&rest[after_key..redact_start]);
        result.push_str("[redacted]");
        rest = &rest[value_end..];
        lower = rest.to_ascii_lowercase();
    }
}

/// Replaces the contents of every quoted string, whatever the quoting.
///
/// A parser error can quote the offending value, such as a display name or a
/// Provider reply. The quotes stay; their contents do not.
fn redact_quoted_values(message: &str) -> String {
    let mut redacted = String::with_capacity(message.len());
    let mut cursor = 0;
    while let Some((offset, character)) = message[cursor..]
        .char_indices()
        .find(|(_, character)| matches!(character, '"' | '\''))
    {
        let open = cursor + offset;
        redacted.push_str(&message[cursor..open]);
        let (end, closed) = quoted_end(message, open);
        let inner = if closed {
            &message[open + 1..end - 1]
        } else {
            &message[open + 1..end]
        };
        let key_like = inner
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '_' | '-' | '.'));
        let followed_by_separator = message[end..]
            .chars()
            .find(|next| !next.is_whitespace())
            .is_some_and(|next| matches!(next, ':' | '='));
        redacted.push(character);
        if key_like && followed_by_separator {
            redacted.push_str(inner);
        } else {
            redacted.push_str("[redacted]");
        }
        if closed {
            redacted.push(character);
        }
        cursor = end;
    }
    redacted.push_str(&message[cursor..]);

    redacted
}

/// Returns the byte after a closing quote and whether a closing quote exists.
/// A backslash escapes the next character, so JSON's `\"` stays inside the
/// string.
fn quoted_end(text: &str, open: usize) -> (usize, bool) {
    let quote = text.as_bytes()[open] as char;
    let mut escaped = false;
    for (offset, character) in text[open + 1..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if character == quote {
            return (open + 1 + offset + 1, true);
        }
    }

    (text.len(), false)
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }

    let mut truncated = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::{sanitize_value, scrub_error};

    #[test]
    fn an_error_keeps_its_meaning_without_credentials() {
        assert_eq!(
            scrub_error("the provider answered 401 for api_key=sk-live-123"),
            "the provider answered 401 for api_key=[redacted]"
        );
        assert_eq!(
            scrub_error("cannot reach https://user:pass@example.com/hook?token=abc#frag"),
            "cannot reach https://example.com/[redacted]"
        );
        assert_eq!(
            scrub_error("Authorization: Bearer abc.def.ghi"),
            "Authorization: [redacted]"
        );
    }

    #[test]
    fn a_quoted_or_header_credential_is_fully_removed() {
        assert_eq!(
            scrub_error("password=\"CANARY\" and the rest"),
            "password=[redacted] and the rest"
        );
        assert_eq!(
            scrub_error("Cookie: session=CANARY; theme=dark"),
            "Cookie: [redacted]"
        );
        assert_eq!(
            scrub_error("{\"password\":\"CANARY\"}"),
            "{\"password\":[redacted]}"
        );
        assert_eq!(
            scrub_error("invalid type: string \"CANARY\", expected u64"),
            "invalid type: string \"[redacted]\", expected u64"
        );
    }

    #[test]
    fn an_escaped_quote_does_not_end_the_secret() {
        assert_eq!(
            scrub_error("{\"password\":\"before\\\"CANARY_AFTER\"}"),
            "{\"password\":[redacted]}"
        );
    }

    #[test]
    fn a_path_with_spaces_loses_its_private_pieces() {
        let scrubbed = scrub_error("cannot write to /home/user/My Private Folder/file: disk full");
        assert_eq!(
            scrubbed,
            "cannot write to [redacted] [redacted] [redacted] disk full"
        );
    }

    #[test]
    fn a_multibyte_character_is_not_mistaken_for_a_quote() {
        // `Ģ` truncates to the ASCII quote byte; a byte-wise scan would slice
        // inside it and panic while the panic hook is running.
        let scrubbed = scrub_error("password=ĢCANARYĢ and provider=ĢxĢ");
        assert!(scrubbed.contains("password=[redacted]"));
        assert!(!scrubbed.contains("CANARY"));
    }

    #[test]
    fn an_absolute_path_is_removed() {
        assert_eq!(
            scrub_error("cannot write /home/user/Neha Workspace/cache.json"),
            "cannot write [redacted] [redacted]"
        );
    }

    #[test]
    fn a_response_keeps_an_inbound_trace_or_starts_one() {
        super::install_propagator();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "traceparent",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
                .parse()
                .unwrap(),
        );
        let inbound = super::inbound_parent(&headers);
        let echoed = super::response_traceparent(&tracing::Span::none(), &inbound);
        assert!(echoed.starts_with("00-4bf92f3577b34da6a3ce929d0e0e4736-"));
        assert_eq!(echoed.rsplit('-').next(), Some("01"));

        let root =
            super::response_traceparent(&tracing::Span::none(), &opentelemetry::Context::new());
        let parts: Vec<&str> = root.split('-').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[1].len(), 32);
        assert_eq!(parts[2].len(), 16);
    }

    #[test]
    fn an_error_cannot_inject_a_log_line() {
        let scrubbed = scrub_error("one\ntwo\rthree\u{1b}[31m");
        assert!(!scrubbed.contains('\n'));
        assert!(!scrubbed.contains('\r'));
        assert!(!scrubbed.contains('\u{1b}'));
    }

    #[test]
    fn a_long_error_is_bounded() {
        let long = "a".repeat(4_000);
        assert_eq!(scrub_error(&long).chars().count(), 1024);
    }

    #[test]
    fn a_user_value_is_trimmed_and_bounded() {
        assert_eq!(sanitize_value("  abc\n\ndef  "), "abc  def");
        let long = "b".repeat(400);
        assert_eq!(sanitize_value(&long).chars().count(), 256);
    }
}
