// SPDX-License-Identifier: Apache-2.0
//! The **outbound fetch gate** — plain Rust, no Wasmtime types — that decides
//! whether a `network:fetch` plugin's outbound HTTP request is allowed.
//!
//! This is a faithful Rust port of the gate in the hellohq Flutter app's
//! `PluginNetworkService` (`lib/app/utils/service/plugin_network_service.dart`):
//!   - **scheme** must be `https` (https-only),
//!   - **host** must be covered by the plugin's declared **origin allowlist**
//!     (exact match or `*.suffix` wildcard, case-insensitive; an EMPTY allowlist
//!     denies everything — the misconfiguration guard),
//!   - if the host is an **IP literal** it must not be a private / loopback /
//!     link-local / cloud-metadata address ([`is_blocked_address`], the H4/H5
//!     SSRF block set).
//!
//! It is wired into the `wasi:http@0.2` host hooks in [`crate::wasi_guests`]
//! (`GatedHttpHooks`) so a JS/Go guest's outbound `wasi:http` send runs through
//! this gate before the real request leaves the host.
//!
//! ## Residual (documented, same as the Dart source)
//! A literal-IP host is checked directly here. A DNS **hostname** is allowed
//! through on the address check (we cannot resolve it without doing network I/O
//! in the gate, and the allowlist already bounds which hosts can be reached); a
//! DNS-rebinding TOCTOU between this check and the actual connect therefore
//! remains, bounded by the origin allowlist. Closing it fully would require
//! resolving and pinning the connection to the validated address — a follow-on.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Why an outbound request was refused by the gate. Maps to a `wasi:http`
/// `ErrorCode` at the hook boundary (see [`crate::wasi_guests`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchDenial {
    /// The scheme was not `https`.
    SchemeBlocked,
    /// The host is not covered by the plugin's origin allowlist (or the
    /// allowlist is empty — deny-all).
    OriginBlocked,
    /// The host is an IP literal in the private / loopback / link-local /
    /// metadata block set (SSRF).
    AddressBlocked,
}

/// True when `ip` is **not** a public, routable address — the set
/// `network:fetch` refuses. Mirrors the Dart `isBlockedFetchAddress` EXACTLY.
///
/// IPv4 blocked: `0.0.0.0/8`, `10.0.0.0/8`, `100.64.0.0/10` (CGNAT),
/// `127.0.0.0/8` (loopback), `169.254.0.0/16` (link-local incl. the
/// `169.254.169.254` cloud-metadata endpoint), `172.16.0.0/12`,
/// `192.168.0.0/16`, `255.255.255.255`; also loopback / link-local / multicast /
/// unspecified / broadcast.
///
/// IPv6 blocked: `::` (unspecified), `::1` (loopback), `fc00::/7` (ULA),
/// link-local (`fe80::/10`), multicast; an IPv4-mapped `::ffff:a.b.c.d`
/// re-checks the embedded IPv4.
pub fn is_blocked_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => is_blocked_v6(v6),
    }
}

fn is_blocked_v4(v4: Ipv4Addr) -> bool {
    // loopback / link-local / multicast / unspecified / broadcast — the
    // categorical checks the Dart `addr.isLoopback || isLinkLocal || isMulticast`
    // covers, plus the explicit prefixes below.
    if v4.is_loopback()
        || v4.is_link_local()
        || v4.is_multicast()
        || v4.is_unspecified()
        || v4.is_broadcast()
    {
        return true;
    }
    let [b0, b1, _, _] = v4.octets();
    if b0 == 0 {
        return true; // 0.0.0.0/8 "this host"
    }
    if b0 == 10 {
        return true; // 10.0.0.0/8
    }
    if b0 == 100 && (64..=127).contains(&b1) {
        return true; // 100.64.0.0/10 CGNAT
    }
    if b0 == 127 {
        return true; // 127.0.0.0/8 (also is_loopback)
    }
    if b0 == 169 && b1 == 254 {
        return true; // 169.254.0.0/16 (cloud metadata 169.254.169.254)
    }
    if b0 == 172 && (16..=31).contains(&b1) {
        return true; // 172.16.0.0/12
    }
    if b0 == 192 && b1 == 168 {
        return true; // 192.168.0.0/16
    }
    // 255.255.255.255 is already covered by is_broadcast; kept implicit.
    false
}

fn is_blocked_v6(v6: Ipv6Addr) -> bool {
    if v6.is_unspecified() || v6.is_loopback() || v6.is_multicast() {
        return true;
    }
    // IPv4-mapped ::ffff:a.b.c.d → re-check the embedded IPv4.
    if let Some(v4) = v6.to_ipv4_mapped() {
        return is_blocked_v4(v4);
    }
    let seg0 = v6.segments()[0];
    if (seg0 & 0xFE00) == 0xFC00 {
        return true; // fc00::/7 unique-local
    }
    if (seg0 & 0xFFC0) == 0xFE80 {
        return true; // fe80::/10 link-local
    }
    false
}

/// True when `host` (no scheme/path) is covered by `allowlist`.
///
/// An entry `"example.com"` matches exactly `example.com`. An entry
/// `"*.example.com"` matches `example.com` itself and any sub-domain. Matching
/// is case-insensitive. An EMPTY allowlist matches nothing (deny-all — the safe
/// default / misconfiguration guard).
pub fn origin_allowed(host: &str, allowlist: &[String]) -> bool {
    let host = host.to_ascii_lowercase();
    for origin in allowlist {
        let norm = origin.to_ascii_lowercase();
        if norm == host {
            return true;
        }
        if let Some(suffix) = norm.strip_prefix("*.") {
            // `*.example.com` matches `example.com` and `*.example.com`.
            if host == suffix || host.ends_with(&format!(".{suffix}")) {
                return true;
            }
        }
    }
    false
}

/// Run the full gate for an outbound request. Combines, in order:
///   1. `scheme` must be `https` → else [`FetchDenial::SchemeBlocked`],
///   2. `host` must be origin-allowed → else [`FetchDenial::OriginBlocked`],
///   3. if `host` is an IP literal it must not be [`is_blocked_address`] → else
///      [`FetchDenial::AddressBlocked`].
///
/// A DNS hostname (non-literal) passes step 3; the DNS-resolution-time SSRF
/// re-check is the documented residual (see module docs).
pub fn check_request(scheme: &str, host: &str, allowlist: &[String]) -> Result<(), FetchDenial> {
    if !scheme.eq_ignore_ascii_case("https") {
        return Err(FetchDenial::SchemeBlocked);
    }
    if !origin_allowed(host, allowlist) {
        return Err(FetchDenial::OriginBlocked);
    }
    // `localhost` (and `*.localhost`) is a loopback host even though it is not a
    // literal IP — block it explicitly, mirroring the Dart guard.
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") {
        return Err(FetchDenial::AddressBlocked);
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_blocked_address(ip) {
            return Err(FetchDenial::AddressBlocked);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn metadata_endpoint_blocked() {
        assert!(is_blocked_address(ip("169.254.169.254")));
    }

    #[test]
    fn loopback_blocked() {
        assert!(is_blocked_address(ip("127.0.0.1")));
        assert!(is_blocked_address(ip("::1")));
    }

    #[test]
    fn private_ranges_blocked() {
        assert!(is_blocked_address(ip("10.0.0.5")));
        assert!(is_blocked_address(ip("192.168.1.1")));
        assert!(is_blocked_address(ip("172.16.0.1")));
        assert!(is_blocked_address(ip("100.64.0.1"))); // CGNAT
    }

    #[test]
    fn extra_v4_edges_blocked() {
        assert!(is_blocked_address(ip("0.0.0.0")));
        assert!(is_blocked_address(ip("169.254.0.1"))); // link-local
        assert!(is_blocked_address(ip("255.255.255.255"))); // broadcast
        assert!(is_blocked_address(ip("172.31.255.255"))); // top of /12
    }

    #[test]
    fn v6_ula_blocked() {
        assert!(is_blocked_address(ip("fd00::1"))); // fc00::/7 ULA
        assert!(is_blocked_address(ip("fe80::1"))); // link-local
        assert!(is_blocked_address(ip("::"))); // unspecified
    }

    #[test]
    fn v4_mapped_blocked() {
        assert!(is_blocked_address(ip("::ffff:127.0.0.1")));
        assert!(is_blocked_address(ip("::ffff:10.0.0.1")));
    }

    #[test]
    fn public_addresses_allowed() {
        assert!(!is_blocked_address(ip("8.8.8.8")));
        assert!(!is_blocked_address(ip("1.1.1.1")));
        // A public v4-mapped is fine.
        assert!(!is_blocked_address(ip("::ffff:8.8.8.8")));
    }

    #[test]
    fn just_outside_private_ranges_allowed() {
        assert!(!is_blocked_address(ip("11.0.0.1")));
        assert!(!is_blocked_address(ip("172.32.0.1"))); // just past /12
        assert!(!is_blocked_address(ip("100.128.0.1"))); // just past CGNAT /10
        assert!(!is_blocked_address(ip("192.169.0.1")));
    }

    #[test]
    fn origin_exact_match() {
        let allow = vec!["api.example.com".to_string()];
        assert!(origin_allowed("api.example.com", &allow));
        assert!(origin_allowed("API.EXAMPLE.COM", &allow)); // case-insensitive
        assert!(!origin_allowed("evil.example.com", &allow));
        assert!(!origin_allowed("xapi.example.com", &allow));
    }

    #[test]
    fn origin_wildcard_match() {
        let allow = vec!["*.example.com".to_string()];
        assert!(origin_allowed("example.com", &allow)); // bare suffix
        assert!(origin_allowed("api.example.com", &allow));
        assert!(origin_allowed("a.b.example.com", &allow));
        assert!(!origin_allowed("example.org", &allow));
        assert!(!origin_allowed("notexample.com", &allow));
    }

    #[test]
    fn empty_allowlist_denies_all() {
        assert!(!origin_allowed("api.example.com", &[]));
    }

    #[test]
    fn check_request_https_only() {
        let allow = vec!["api.example.com".to_string()];
        assert_eq!(
            check_request("http", "api.example.com", &allow),
            Err(FetchDenial::SchemeBlocked)
        );
        assert!(check_request("https", "api.example.com", &allow).is_ok());
    }

    #[test]
    fn check_request_origin_block() {
        let allow = vec!["api.example.com".to_string()];
        assert_eq!(
            check_request("https", "evil.example.com", &allow),
            Err(FetchDenial::OriginBlocked)
        );
    }

    #[test]
    fn check_request_ssrf_literal_block() {
        // An allowlisted host that is itself a blocked IP literal.
        let allow = vec!["169.254.169.254".to_string()];
        assert_eq!(
            check_request("https", "169.254.169.254", &allow),
            Err(FetchDenial::AddressBlocked)
        );
        let allow_lo = vec!["127.0.0.1".to_string()];
        assert_eq!(
            check_request("https", "127.0.0.1", &allow_lo),
            Err(FetchDenial::AddressBlocked)
        );
    }

    #[test]
    fn check_request_localhost_block() {
        let allow = vec!["localhost".to_string()];
        assert_eq!(
            check_request("https", "localhost", &allow),
            Err(FetchDenial::AddressBlocked)
        );
    }

    #[test]
    fn check_request_public_literal_allowed() {
        let allow = vec!["8.8.8.8".to_string()];
        assert!(check_request("https", "8.8.8.8", &allow).is_ok());
    }
}
