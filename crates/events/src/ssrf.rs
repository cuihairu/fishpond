//! SSRF guard for webhook endpoints (F018).
//!
//! Webhooks turn stored events into outbound requests, which makes the
//! endpoint URL an attack surface: a tenant pointing a subscription at
//! cloud metadata (`169.254.169.254`), loopback services, or the
//! cluster's internal names would make *fishpond* perform the request
//! from inside the network. Registration therefore fails closed unless
//! the endpoint is:
//!
//! * `https` (plaintext HTTP never leaves the process), with no
//!   userinfo (credentials in a URL leak into logs) and no fragment;
//! * addressed by a domain name — **not** an IP literal in a private,
//!   loopback, link-local, unique-local, unspecified or reserved range;
//! * not a special-use name (`localhost`, `*.localhost`, `*.local`,
//!   `*.internal`).
//!
//! Known limitation, by design: a domain can still *resolve* to a
//! private address. Checking that needs DNS at request time, which is
//! real network I/O — the F032+ transport re-validates the resolved
//! address just before connecting, and this guard keeps everything
//! that can be decided without I/O out of the database.

use url::Url;

/// Why an endpoint was rejected. The error names the rule, never the
/// URL's userinfo (which is rejected before it could contain anything).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointError {
    /// Not a parseable URL.
    Unparseable,
    /// The scheme must be `https`.
    Scheme,
    /// Credentials must not travel in the URL.
    Userinfo,
    /// Fragments have no meaning on a webhook endpoint.
    Fragment,
    /// The host is an IP literal in a forbidden range.
    ForbiddenIp,
    /// The host is a special-use name (localhost & friends).
    ForbiddenHost,
    /// No usable host at all.
    NoHost,
}

impl std::fmt::Display for EndpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let why = match self {
            EndpointError::Unparseable => "not a parseable URL",
            EndpointError::Scheme => "scheme must be https",
            EndpointError::Userinfo => "userinfo (user:password@) is not allowed",
            EndpointError::Fragment => "fragments are not allowed",
            EndpointError::ForbiddenIp => {
                "IP literal is in a private, loopback, link-local or reserved range"
            }
            EndpointError::ForbiddenHost => "host is a special-use name (localhost and friends)",
            EndpointError::NoHost => "no host",
        };
        write!(f, "webhook endpoint rejected: {why}")
    }
}

impl std::error::Error for EndpointError {}

/// Special-use suffixes a webhook host must not end with (RFC 6761/6762
/// plus the internal-TLD conventions).
const FORBIDDEN_SUFFIXES: [&str; 3] = [".localhost", ".local", ".internal"];

/// Validates a webhook endpoint, returning the normalized URL.
pub fn validate_endpoint(raw: &str) -> Result<Url, EndpointError> {
    let url = Url::parse(raw).map_err(|_| EndpointError::Unparseable)?;
    if url.scheme() != "https" {
        return Err(EndpointError::Scheme);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(EndpointError::Userinfo);
    }
    if url.fragment().is_some() {
        return Err(EndpointError::Fragment);
    }
    // IP literals (v4 and v6 — the url crate hands brackets to us as
    // typed hosts) are checked against the non-routable sets; domain
    // names go through the special-use suffix list.
    use url::Host;
    match url.host() {
        Some(Host::Ipv4(ip)) => {
            if is_forbidden_ip(std::net::IpAddr::V4(ip)) {
                return Err(EndpointError::ForbiddenIp);
            }
        }
        Some(Host::Ipv6(ip)) => {
            if is_forbidden_ip(std::net::IpAddr::V6(ip)) {
                return Err(EndpointError::ForbiddenIp);
            }
        }
        Some(Host::Domain(domain)) => {
            let host = domain.trim_end_matches('.').to_ascii_lowercase();
            if host.is_empty() {
                return Err(EndpointError::NoHost);
            }
            if host == "localhost" || FORBIDDEN_SUFFIXES.iter().any(|s| host.ends_with(s)) {
                return Err(EndpointError::ForbiddenHost);
            }
        }
        None => return Err(EndpointError::NoHost),
    }
    Ok(url)
}

/// The IP ranges fishpond must never request: loopback, private,
/// link-local (includes cloud metadata), unique-local, unspecified,
/// broadcast, and the IETF documentation ranges.
fn is_forbidden_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unique_local()
                || v6.is_unspecified()
                || (v6.is_unicast_link_local())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EndpointError, validate_endpoint};

    #[test]
    fn public_https_endpoints_pass() {
        assert!(validate_endpoint("https://hooks.example.com/fishpond").is_ok());
        assert!(validate_endpoint("https://hooks.example.com:8443/hook").is_ok());
        assert!(validate_endpoint("https://webhooks.partner.example.co.uk/in").is_ok());
    }

    #[test]
    fn plaintext_and_url_tricks_are_rejected() {
        assert_eq!(
            validate_endpoint("http://hooks.example.com/x"),
            Err(EndpointError::Scheme)
        );
        assert_eq!(
            validate_endpoint("https://user:pass@hooks.example.com/x"),
            Err(EndpointError::Userinfo)
        );
        assert_eq!(
            validate_endpoint("https://hooks.example.com/x#frag"),
            Err(EndpointError::Fragment)
        );
        assert_eq!(
            validate_endpoint("not a url"),
            Err(EndpointError::Unparseable)
        );
        assert_eq!(
            validate_endpoint("https://"),
            Err(EndpointError::Unparseable)
        );
    }

    #[test]
    fn dangerous_ip_literals_are_rejected() {
        for raw in [
            "https://127.0.0.1/hook",
            "https://10.0.0.5/hook",
            "https://192.168.1.1/hook",
            "https://172.16.0.9/hook",
            "https://169.254.169.254/latest/meta-data",
            "https://0.0.0.0/hook",
            "https://[::1]/hook",
            "https://[fe80::1]/hook",
            "https://[fc00::1]/hook",
        ] {
            assert_eq!(
                validate_endpoint(raw),
                Err(EndpointError::ForbiddenIp),
                "must reject {raw}"
            );
        }
    }

    #[test]
    fn special_use_names_are_rejected() {
        for raw in [
            "https://localhost/hook",
            "https://localhost.:8443/hook",
            "https://metadata.local/hook",
            "https://api.internal/hook",
            "https://my.service.localhost/hook",
        ] {
            assert_eq!(
                validate_endpoint(raw),
                Err(EndpointError::ForbiddenHost),
                "must reject {raw}"
            );
        }
    }
}
