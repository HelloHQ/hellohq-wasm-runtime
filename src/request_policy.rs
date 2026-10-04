// SPDX-License-Identifier: Apache-2.0
//! What a plugin may put on — and get back from — an outbound HTTP request.
//! Plain Rust, no Wasmtime types.
//!
//! This is a port of the hellohq app's `PluginRequestPolicy`
//! (`lib/app/utils/service/plugin_request_policy.dart`, doc 30 §2.2). The app
//! applies that policy inside `PluginNetworkService.fetch`, which every plugin
//! network path goes through EXCEPT the in-host `wasi:http@0.2` path
//! (`wasi_guests::GatedHttpHooks`): a JS/Go guest's request is sent by
//! hyper from Rust and never reaches Dart. That path applies this module
//! instead, so both hosts hand a plugin the same treatment.
//!
//! **Request headers** are a strict allowlist: a plugin may set only the
//! content-negotiation and conditional-request headers in
//! [`ALLOWED_REQUEST_HEADERS`]. Everything else is dropped — explicitly
//! including `Authorization`, `Proxy-Authorization`, `Cookie`, vendor
//! API-key/signature headers and any name matching a credential pattern
//! ([`is_credential_header_name`]).
//!
//! **Response headers**: [`STRIPPED_RESPONSE_HEADERS`] (`Set-Cookie`,
//! `Set-Cookie2`) never reach the plugin.
//!
//! ## Keeping the two lists in sync
//! The lists below must match the Dart constants exactly. Both test suites read
//! one case table, `tests/fixtures/plugin_request_policy_cases.txt` (canonical
//! here; vendored byte-identical into hellohq as
//! `test/fixtures/plugin_request_policy_cases.txt`). A change to either policy
//! has to change that table, and the other repo's test then fails until its
//! policy and its vendored copy are updated to match.
//!
//! The `wasi:http@0.3` path is deliberately NOT filtered here: it frames the
//! request to Dart, whose servicer applies `PluginRequestPolicy` (and, per doc
//! 30 §4.6, will read the reserved `x-hellohq-credential` handle header on that
//! path — so the runtime must forward it untouched).

/// Lower-case request header names a plugin may set. Mirrors
/// `PluginRequestPolicy.allowedRequestHeaders`.
pub const ALLOWED_REQUEST_HEADERS: [&str; 5] = [
    "accept",
    "accept-language",
    "content-type",
    "if-none-match",
    "if-modified-since",
];

/// Lower-case response header names never handed back to a plugin. Mirrors
/// `PluginRequestPolicy.strippedResponseHeaders`.
pub const STRIPPED_RESPONSE_HEADERS: [&str; 2] = ["set-cookie", "set-cookie2"];

/// Substrings that mark a header name as credential-bearing. Mirrors
/// `PluginRequestPolicy._credentialFragments` (same order).
const CREDENTIAL_FRAGMENTS: [&str; 10] = [
    "auth",
    "cookie",
    "key",
    "secret",
    "sign",
    "token",
    "passphrase",
    "password",
    "session",
    "cert",
];

/// Vendor prefixes whose every header is part of a signed request (Coinbase
/// `CB-ACCESS-*`, OKX `OK-ACCESS-*`), including the non-secret timestamp.
/// Mirrors `PluginRequestPolicy._credentialPrefixes`.
const CREDENTIAL_PREFIXES: [&str; 2] = ["cb-access-", "ok-access-"];

/// The reserved request header that carries a credential-vault handle (doc 30
/// §4.6). Only the `wasi:http@0.3` path (serviced by Dart) may carry it; the
/// in-host `wasi:http@0.2` path refuses a request carrying it outright
/// (`credential_unsupported_transport`) rather than dropping it, since that
/// path never leaves Rust and must not gain secrets.
pub const RESERVED_CREDENTIAL_HEADER: &str = "x-hellohq-credential";

/// True when `name` looks like it carries a credential — a named vendor header
/// or any name containing a credential fragment. Case-insensitive; surrounding
/// whitespace is ignored. Mirrors `PluginRequestPolicy.isCredentialHeaderName`.
pub fn is_credential_header_name(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    CREDENTIAL_PREFIXES.iter().any(|p| n.starts_with(p))
        || CREDENTIAL_FRAGMENTS.iter().any(|f| n.contains(f))
}

/// True when a plugin may set request header `name`: it is allowlisted and
/// does not look like a credential. Case-insensitive.
pub fn is_allowed_request_header(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    ALLOWED_REQUEST_HEADERS.contains(&n.as_str()) && !is_credential_header_name(&n)
}

/// True when response header `name` must be removed before the plugin sees
/// the response. Case-insensitive.
pub fn is_stripped_response_header(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    STRIPPED_RESPONSE_HEADERS.contains(&n.as_str())
}

/// True when `name` is [`RESERVED_CREDENTIAL_HEADER`]. Case-insensitive.
pub fn is_reserved_credential_header(name: &str) -> bool {
    name.trim().eq_ignore_ascii_case(RESERVED_CREDENTIAL_HEADER)
}

/// Result of [`sanitize_request_headers`]. Mirrors the Dart `SanitizedHeaders`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizedHeaders<V> {
    /// The headers that will be sent (lower-case names, input order).
    pub headers: Vec<(String, V)>,
    /// How many plugin headers were dropped.
    pub dropped: usize,
    /// Whether any dropped header matched a credential pattern.
    pub dropped_credential: bool,
}

/// Filter plugin-supplied request `headers` down to the allowlist. Names are
/// returned lower-cased. `dropped` / `dropped_credential` are for a
/// content-blind count — never log the names or values themselves. Mirrors
/// `PluginRequestPolicy.sanitizeRequestHeaders`.
pub fn sanitize_request_headers<N, V, I>(headers: I) -> SanitizedHeaders<V>
where
    N: AsRef<str>,
    I: IntoIterator<Item = (N, V)>,
{
    let mut kept = Vec::new();
    let mut dropped = 0;
    let mut dropped_credential = false;
    for (name, value) in headers {
        let n = name.as_ref().trim().to_ascii_lowercase();
        if is_allowed_request_header(&n) {
            kept.push((n, value));
        } else {
            dropped += 1;
            if is_credential_header_name(&n) {
                dropped_credential = true;
            }
        }
    }
    SanitizedHeaders {
        headers: kept,
        dropped,
        dropped_credential,
    }
}

/// Remove cookie-setting headers from an upstream response. Mirrors
/// `PluginRequestPolicy.sanitizeResponseHeaders`.
pub fn sanitize_response_headers<N, V, I>(headers: I) -> Vec<(N, V)>
where
    N: AsRef<str>,
    I: IntoIterator<Item = (N, V)>,
{
    headers
        .into_iter()
        .filter(|(name, _)| !is_stripped_response_header(name.as_ref()))
        .collect()
}

#[cfg(test)]
mod tests {
    //! Case-for-case port of hellohq `test/unit/service/
    //! plugin_request_policy_test.dart`, plus the shared case table both suites
    //! read.
    use super::*;

    // ── PluginRequestPolicy.sanitizeRequestHeaders ──────────────────────────

    #[test]
    fn keeps_exactly_the_allowlisted_headers_case_insensitively() {
        let r = sanitize_request_headers([
            ("Accept", "application/json"),
            ("accept-language", "en-GB"),
            ("Content-Type", "application/json"),
            ("IF-NONE-MATCH", "\"etag-1\""),
            ("If-Modified-Since", "Wed, 21 Oct 2015 07:28:00 GMT"),
        ]);
        assert_eq!(
            r.headers,
            vec![
                ("accept".to_string(), "application/json"),
                ("accept-language".to_string(), "en-GB"),
                ("content-type".to_string(), "application/json"),
                ("if-none-match".to_string(), "\"etag-1\""),
                (
                    "if-modified-since".to_string(),
                    "Wed, 21 Oct 2015 07:28:00 GMT"
                ),
            ]
        );
        assert_eq!(r.dropped, 0);
        assert!(!r.dropped_credential);
    }

    #[test]
    fn drops_every_named_credential_header() {
        let credential_headers = [
            ("Authorization", "Bearer secret"),
            ("Proxy-Authorization", "Basic c2VjcmV0"),
            ("Cookie", "session=secret"),
            ("X-API-Key", "secret"),
            ("API-Key", "secret"),
            ("API-Sign", "secret"),
            ("CB-ACCESS-KEY", "secret"),
            ("CB-ACCESS-SIGN", "secret"),
            ("CB-ACCESS-TIMESTAMP", "1"),
            ("CB-ACCESS-PASSPHRASE", "secret"),
            ("OK-ACCESS-KEY", "secret"),
            ("OK-ACCESS-SIGN", "secret"),
            ("OK-ACCESS-PASSPHRASE", "secret"),
            ("X-MBX-APIKEY", "secret"),
        ];
        let mut input = credential_headers.to_vec();
        input.push(("Accept", "application/json"));
        let r = sanitize_request_headers(input);
        assert_eq!(r.headers, vec![("accept".to_string(), "application/json")]);
        assert_eq!(r.dropped, credential_headers.len());
        assert!(r.dropped_credential);
        for (name, _) in credential_headers {
            assert!(
                is_credential_header_name(name),
                "{name} must be classified as a credential header"
            );
        }
    }

    #[test]
    fn credential_pattern_names_are_classified_as_credentials() {
        for name in [
            "X-Auth-Token",
            "X-Custom-Secret",
            "X-Signature",
            "Access-Token",
            "AccessKey",
            "Token",
            "X-Passphrase",
            "X-Session-Id",
            "X-Password",
            "X-Client-Cert",
            "X-Api-Key-Id",
        ] {
            assert!(is_credential_header_name(name), "{name}");
        }
        for name in ["Accept", "Content-Type", "X-Request-Id"] {
            assert!(!is_credential_header_name(name), "{name}");
        }
    }

    #[test]
    fn drops_host_controlled_and_unknown_headers_too() {
        let r = sanitize_request_headers([
            ("Host", "internal.example"),
            ("Content-Length", "999"),
            ("Transfer-Encoding", "chunked"),
            ("Connection", "keep-alive"),
            ("User-Agent", "plugin/1.0"),
            ("X-Request-Id", "abc"),
            ("Origin", "https://evil.example"),
        ]);
        assert!(r.headers.is_empty());
        assert_eq!(r.dropped, 7);
        assert!(!r.dropped_credential);
    }

    #[test]
    fn an_empty_map_is_a_no_op() {
        let r = sanitize_request_headers(Vec::<(&str, &str)>::new());
        assert!(r.headers.is_empty());
        assert_eq!(r.dropped, 0);
    }

    // ── PluginRequestPolicy.sanitizeResponseHeaders ─────────────────────────

    #[test]
    fn strips_set_cookie_any_case_and_keeps_the_rest() {
        let out = sanitize_response_headers([
            ("content-type", "application/json"),
            ("set-cookie", "session=abc; HttpOnly"),
            ("Set-Cookie2", "legacy=1"),
            ("etag", "\"v1\""),
        ]);
        assert_eq!(
            out,
            vec![("content-type", "application/json"), ("etag", "\"v1\"")]
        );
    }

    // ── Runtime-only: the reserved credential-handle header ────────────────

    #[test]
    fn reserved_credential_header_is_recognised_and_never_allowed() {
        for name in [
            "x-hellohq-credential",
            "X-HelloHQ-Credential",
            " x-hellohq-credential ",
        ] {
            assert!(is_reserved_credential_header(name), "{name:?}");
            assert!(!is_allowed_request_header(name), "{name:?}");
        }
        assert!(!is_reserved_credential_header("x-hellohq-credentials"));
        assert!(!is_reserved_credential_header("authorization"));
    }

    // ── The shared case table (also read by the hellohq Dart suite) ────────

    /// The canonical case table. Each non-comment line is
    /// `<expectation> <header-name>`; see the file header for the vocabulary.
    const CASES: &str = include_str!("../tests/fixtures/plugin_request_policy_cases.txt");

    #[test]
    fn shared_case_table_matches_the_policy() {
        let mut checked = 0;
        for (lineno, raw) in CASES.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (expectation, name) = line
                .split_once(char::is_whitespace)
                .unwrap_or_else(|| panic!("line {}: malformed {raw:?}", lineno + 1));
            let name = name.trim();
            let at = format!("line {}: {expectation} {name}", lineno + 1);
            let sanitized = sanitize_request_headers([(name, ())]);
            match expectation {
                // Sent as-is (allowlisted, not credential-looking).
                "keep" => {
                    assert!(is_allowed_request_header(name), "{at}");
                    assert_eq!(sanitized.dropped, 0, "{at}");
                    assert!(!is_credential_header_name(name), "{at}");
                }
                // Dropped, and counted as a credential.
                "drop-credential" => {
                    assert!(!is_allowed_request_header(name), "{at}");
                    assert_eq!(sanitized.dropped, 1, "{at}");
                    assert!(sanitized.dropped_credential, "{at}");
                    assert!(is_credential_header_name(name), "{at}");
                }
                // Dropped, not credential-looking.
                "drop" => {
                    assert!(!is_allowed_request_header(name), "{at}");
                    assert_eq!(sanitized.dropped, 1, "{at}");
                    assert!(!sanitized.dropped_credential, "{at}");
                    assert!(!is_credential_header_name(name), "{at}");
                }
                "response-strip" => assert!(is_stripped_response_header(name), "{at}"),
                "response-keep" => assert!(!is_stripped_response_header(name), "{at}"),
                other => panic!("{at}: unknown expectation {other:?}"),
            }
            checked += 1;
        }
        // Guard against the table silently going empty (a bad include path).
        assert!(checked >= 40, "only {checked} cases in the shared table");
    }

    /// Every allowlisted header has a `keep` row and every stripped response
    /// header a `response-strip` row, so a list change cannot skip the table.
    #[test]
    fn shared_case_table_covers_both_lists() {
        let rows: Vec<(&str, String)> = CASES
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(|l| l.split_once(char::is_whitespace))
            .map(|(e, n)| (e, n.trim().to_ascii_lowercase()))
            .collect();
        for h in ALLOWED_REQUEST_HEADERS {
            assert!(
                rows.iter().any(|(e, n)| *e == "keep" && n == h),
                "no keep row for {h}"
            );
        }
        let keep_rows = rows.iter().filter(|(e, _)| *e == "keep").count();
        assert_eq!(
            keep_rows,
            ALLOWED_REQUEST_HEADERS.len(),
            "keep rows must be exactly the allowlist, one row each"
        );
        for h in STRIPPED_RESPONSE_HEADERS {
            assert!(
                rows.iter().any(|(e, n)| *e == "response-strip" && n == h),
                "no response-strip row for {h}"
            );
        }
    }
}
