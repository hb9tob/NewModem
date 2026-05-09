//! Parsing for the user-supplied iiod target string.
//!
//! Pluto's URI conventions allow several spellings for the same thing,
//! and we want all of them to work in the GUI's "Pluto address" field
//! and the CLI's `--pluto-uri` flag without the user having to memorize
//! a canonical form. This module accepts:
//!
//! | Input | Meaning |
//! |---|---|
//! | `ip:pluto.local` | mDNS hostname, default port 30431 |
//! | `ip:pluto.local:31000` | mDNS hostname, custom port |
//! | `ip:192.168.2.1` | IPv4, default port |
//! | `ip:192.168.2.1:31000` | IPv4, custom port |
//! | `pluto.local` | Bare hostname (the `ip:` prefix is implied) |
//! | `192.168.2.1` | Bare IPv4 |
//! | `192.168.2.1:31000` | Bare IPv4 with port |
//!
//! IPv6 with the `[…]:port` form is accepted too, for completeness —
//! Pluto itself is IPv4-only over USB-NCM, but a Pluto+ on a real
//! Ethernet may be reached via v6.
//!
//! `usb:…`, `serial:…`, and `local:…` URIs are explicitly **rejected**
//! — those are libiio C-side transports we no longer support since
//! switching to pure-Rust iiod TCP. The error message tells the user
//! to replace them with `ip:192.168.2.1` (which is in fact what Pluto
//! over USB exposes, via the USB-NCM virtual NIC).

use crate::iiod::error::IiodError;

/// Default TCP port the libiio iiod daemon listens on.
pub const IIOD_DEFAULT_PORT: u16 = 30431;

/// Result of parsing a user-supplied target string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlutoTarget {
    /// Hostname or IP literal — passed straight to
    /// `TcpStream::connect((host.as_str(), port))`. Stored as `String`
    /// so we can keep the original spelling in error messages.
    pub host: String,
    /// TCP port. Defaults to [`IIOD_DEFAULT_PORT`] (30431) unless the
    /// caller wrote `…:<port>`.
    pub port: u16,
}

impl PlutoTarget {
    /// `host:port`, the form `TcpStream::connect` accepts as a string.
    pub fn socket_addr_str(&self) -> String {
        // IPv6 literals must be bracketed.
        if self.host.contains(':') && !self.host.starts_with('[') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// Parse a Pluto / iiod target string. See module docs for the
/// accepted spellings.
pub fn parse_pluto_target(s: &str) -> Result<PlutoTarget, IiodError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(IiodError::BadTarget("empty target".into()));
    }

    // Reject deprecated libiio C-side transports up front with a
    // helpful message. We deliberately don't silently coerce — if the
    // user has `usb:1.6.5` saved in their settings from before the
    // switch, we want them to see why it stopped working.
    for bad in ["usb:", "serial:", "local:"] {
        if trimmed.starts_with(bad) {
            return Err(IiodError::BadTarget(format!(
                "transport '{bad}' is no longer supported — \
                 use 'ip:192.168.2.1' (Pluto over USB-NCM) or \
                 'ip:pluto.local' instead"
            )));
        }
    }

    // Strip the optional `ip:` prefix; what's left is either
    // `host`, `host:port`, `[v6]:port`, or a bare IPv6 literal.
    let body = trimmed.strip_prefix("ip:").unwrap_or(trimmed);

    // Bracketed IPv6: `[2001:db8::1]:31000` or just `[2001:db8::1]`.
    if let Some(rest) = body.strip_prefix('[') {
        let (host, port) = match rest.find(']') {
            Some(idx) => {
                let host = &rest[..idx];
                let after = &rest[idx + 1..];
                let port = if after.is_empty() {
                    IIOD_DEFAULT_PORT
                } else if let Some(p) = after.strip_prefix(':') {
                    parse_port(p)?
                } else {
                    return Err(IiodError::BadTarget(format!(
                        "garbage after ']' in {trimmed:?}"
                    )));
                };
                (host.to_string(), port)
            }
            None => {
                return Err(IiodError::BadTarget(format!(
                    "unterminated '[' in {trimmed:?}"
                )))
            }
        };
        if host.is_empty() {
            return Err(IiodError::BadTarget("empty IPv6 host".into()));
        }
        return Ok(PlutoTarget { host, port });
    }

    // Unbracketed: split at the LAST colon — but only if the result
    // looks like `<host>:<port>` (port is u16). A bare unbracketed
    // IPv6 literal `2001:db8::1` has multiple colons; we treat that
    // as host-only with default port. Hostnames and IPv4 have at most
    // one colon, so the "last colon" split is unambiguous for them.
    let colon_count = body.bytes().filter(|&b| b == b':').count();
    let (host, port) = match colon_count {
        0 => (body.to_string(), IIOD_DEFAULT_PORT),
        1 => {
            // Could be `host:port` (IPv4 / hostname) — try to parse
            // the trailing part as a port, fall back to "this is
            // actually a v6 literal we mistook" (which is unlikely
            // with one colon, but handle it cleanly anyway).
            let (h, p) = body
                .rsplit_once(':')
                .expect("colon_count == 1 implies one separator");
            match p.parse::<u16>() {
                Ok(port) if !p.is_empty() => (h.to_string(), port),
                _ => (body.to_string(), IIOD_DEFAULT_PORT),
            }
        }
        _ => {
            // Multi-colon → bare IPv6 literal, default port. If the
            // user wanted a custom port on v6 they should have used
            // brackets.
            (body.to_string(), IIOD_DEFAULT_PORT)
        }
    };

    if host.is_empty() {
        return Err(IiodError::BadTarget(format!("no host in {trimmed:?}")));
    }
    Ok(PlutoTarget { host, port })
}

/// Parse the trailing port of a target string — a tiny helper that
/// turns the underlying `ParseIntError` into our `IiodError::BadTarget`
/// so callers don't have to juggle two error types.
fn parse_port(s: &str) -> Result<u16, IiodError> {
    s.parse::<u16>()
        .map_err(|e| IiodError::BadTarget(format!("port {s:?}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round up the canonical happy paths the GUI / CLI will produce.
    /// Each row is `(input, expected_host, expected_port)`.
    #[test]
    fn parses_canonical_spellings() {
        let cases: &[(&str, &str, u16)] = &[
            ("ip:pluto.local", "pluto.local", IIOD_DEFAULT_PORT),
            ("ip:pluto.local:31000", "pluto.local", 31000),
            ("ip:192.168.2.1", "192.168.2.1", IIOD_DEFAULT_PORT),
            ("ip:192.168.2.1:31000", "192.168.2.1", 31000),
            ("pluto.local", "pluto.local", IIOD_DEFAULT_PORT),
            ("192.168.2.1", "192.168.2.1", IIOD_DEFAULT_PORT),
            ("192.168.2.1:31000", "192.168.2.1", 31000),
            // Whitespace trim — easy to fat-finger in the GUI.
            ("  ip:pluto.local  ", "pluto.local", IIOD_DEFAULT_PORT),
        ];
        for (input, host, port) in cases {
            let t = parse_pluto_target(input).unwrap_or_else(|e| {
                panic!("parse failed for {input:?}: {e}")
            });
            assert_eq!(t.host, *host, "host mismatch on {input:?}");
            assert_eq!(t.port, *port, "port mismatch on {input:?}");
        }
    }

    /// IPv6 with brackets. Pluto over USB-NCM is v4-only but a Pluto+
    /// on a real Ethernet might be on v6.
    #[test]
    fn parses_ipv6_brackets() {
        let t = parse_pluto_target("ip:[2001:db8::1]:31000").unwrap();
        assert_eq!(t.host, "2001:db8::1");
        assert_eq!(t.port, 31000);

        let t2 = parse_pluto_target("[2001:db8::1]").unwrap();
        assert_eq!(t2.host, "2001:db8::1");
        assert_eq!(t2.port, IIOD_DEFAULT_PORT);
    }

    /// Bare IPv6 with no brackets and no port → host-only.
    #[test]
    fn parses_bare_ipv6_as_host_only() {
        let t = parse_pluto_target("2001:db8::1").unwrap();
        assert_eq!(t.host, "2001:db8::1");
        assert_eq!(t.port, IIOD_DEFAULT_PORT);
    }

    /// `socket_addr_str()` re-bracketizes IPv6 so it's directly
    /// usable with `TcpStream::connect(addr.parse::<SocketAddr>())`.
    #[test]
    fn socket_addr_str_brackets_ipv6() {
        let t = parse_pluto_target("ip:[2001:db8::1]:31000").unwrap();
        assert_eq!(t.socket_addr_str(), "[2001:db8::1]:31000");

        let t2 = parse_pluto_target("ip:192.168.2.1:31000").unwrap();
        assert_eq!(t2.socket_addr_str(), "192.168.2.1:31000");
    }

    /// Legacy URIs from before the iiod-TCP switch must surface a
    /// clear error instead of silently failing later at connect time.
    #[test]
    fn rejects_legacy_libiio_transports() {
        for legacy in ["usb:1.6.5", "serial:/dev/ttyACM0,115200", "local:"] {
            let err = parse_pluto_target(legacy)
                .expect_err(&format!("expected error for {legacy:?}"));
            let msg = err.to_string();
            assert!(
                msg.contains("ip:"),
                "error message for {legacy:?} should hint at ip: form, got {msg:?}"
            );
        }
    }

    /// Empty / port-only / bracket-imbalanced inputs are user typos.
    #[test]
    fn rejects_garbage() {
        assert!(parse_pluto_target("").is_err());
        assert!(parse_pluto_target("   ").is_err());
        assert!(parse_pluto_target("ip:").is_err());
        assert!(parse_pluto_target("ip:[2001:db8::1").is_err());
    }
}
