use std::collections::HashMap;
use std::net::IpAddr;

use url::Url;

use crate::types::{NetError, Response};

/// Process-wide opt-in via env var. Older flow that issue #4 introduced. The
/// new `--allow-private-network` CLI flag (issue #33) sets a per-client field
/// that is OR'd with this so existing scripts and Docker setups that pin the
/// env var keep working unchanged.
pub fn env_allows_private_network() -> bool {
    matches!(
        std::env::var("TINYBROWSER_ALLOW_PRIVATE_NETWORK")
            .ok()
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// True when `ip` must never be the target of an outbound request from the
/// engine: loopback, RFC1918 private, link-local (incl. the 169.254.169.254
/// cloud-metadata endpoint), broadcast, documentation, the unspecified address
/// (0.0.0.0 / ::, which the OS routes to localhost), IPv6 unique-local
/// (fc00::/7), and any IPv4-mapped/compatible IPv6 form of the above.
pub fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
            {
                return true;
            }
            if let Some(v4) = v6.to_ipv4_mapped().or_else(|| v6.to_ipv4()) {
                return is_forbidden_ip(IpAddr::V4(v4));
            }
            false
        }
    }
}

/// Whether SSL_CERT_FILE / SSL_CERT_DIR request a custom TLS trust store. A
/// variable that is set but empty is treated as unset.
pub(crate) fn custom_cert_store_requested(
    cert_file: Option<&std::ffi::OsStr>,
    cert_dir: Option<&std::ffi::OsStr>,
) -> bool {
    cert_file.is_some_and(|v| !v.is_empty()) || cert_dir.is_some_and(|v| !v.is_empty())
}

/// Explicit policy for private-network access. Using an enum instead of a
/// bare `bool` makes call sites self-documenting: `Allow` vs `Deny` rather
/// than `true`/`false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivateNetworkPolicy {
    Allow,
    Deny,
}

impl From<bool> for PrivateNetworkPolicy {
    fn from(allow: bool) -> Self {
        if allow { Self::Allow } else { Self::Deny }
    }
}

impl PrivateNetworkPolicy {
    pub fn allows_private_network(self) -> bool {
        matches!(self, Self::Allow)
    }
}

pub fn validate_url(url: &Url, policy: PrivateNetworkPolicy) -> Result<(), NetError> {
    let allow_private_network =
        policy.allows_private_network() || env_allows_private_network();
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" && scheme != "file" {
        return Err(NetError::Ssrf(format!(
            "Forbidden URL scheme '{scheme}' - only http, https, and file are allowed"
        )));
    }

    if scheme == "file" || allow_private_network {
        return Ok(());
    }

    if let Some(host) = url.host() {
        match host {
            url::Host::Ipv4(ip) => {
                if is_forbidden_ip(IpAddr::V4(ip)) {
                    return Err(NetError::Ssrf(format!(
                        "Access to private/internal IP address {ip} is not allowed"
                    )));
                }
            }
            url::Host::Ipv6(ip) => {
                if is_forbidden_ip(IpAddr::V6(ip)) {
                    return Err(NetError::Ssrf(format!(
                        "Access to private/internal IPv6 address {ip} is not allowed"
                    )));
                }
            }
            url::Host::Domain(domain) => {
                let lower_domain = domain.to_lowercase();
                if lower_domain == "localhost"
                    || lower_domain.ends_with(".localhost")
                    || lower_domain == "127.0.0.1"
                    || lower_domain == "::1"
                {
                    return Err(NetError::Ssrf(format!(
                        "Access to localhost domain '{domain}' is not allowed"
                    )));
                }
            }
        }
    }

    Ok(())
}

pub(crate) async fn fetch_file_url(
    url: &Url,
    max_response_bytes: usize,
) -> Result<Response, NetError> {
    let path = url
        .to_file_path()
        .map_err(|()| NetError::Network("Invalid file URL".to_string()))?;
    if let Ok(metadata) = tokio::fs::metadata(&path).await {
        if metadata.len() > max_response_bytes as u64 {
            return Err(crate::types::response_too_large(url, max_response_bytes));
        }
    }
    let body = tokio::fs::read(&path)
        .await
        .map_err(|e| NetError::Network(format!("Failed to read file: {e}")))?;
    if body.len() > max_response_bytes {
        return Err(crate::types::response_too_large(url, max_response_bytes));
    }

    let mut headers = HashMap::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let ct = match ext.to_lowercase().as_str() {
            "html" | "htm" => "text/html",
            "css" => "text/css",
            "js" | "mjs" => "application/javascript",
            "json" => "application/json",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "svg" => "image/svg+xml",
            "webp" => "image/webp",
            "ico" => "image/x-icon",
            _ => "application/octet-stream",
        };
        headers.insert("content-type".to_string(), ct.to_string());
    }

    Ok(Response {
        url: url.clone(),
        status: 200,
        headers,
        body,
        redirected_from: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::{is_forbidden_ip, validate_url};
    use std::net::IpAddr;
    use std::str::FromStr;
    use url::Url;

    fn ip(s: &str) -> IpAddr {
        IpAddr::from_str(s).unwrap()
    }

    #[test]
    fn ipv4_private_and_special_ranges_are_forbidden() {
        for s in [
            "127.0.0.1",
            "127.5.6.7",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "255.255.255.255",
            "192.0.2.1",
        ] {
            assert!(is_forbidden_ip(ip(s)), "{s} should be forbidden");
        }
    }

    #[test]
    fn public_ipv4_is_allowed() {
        for s in ["1.1.1.1", "8.8.8.8", "93.184.216.34"] {
            assert!(!is_forbidden_ip(ip(s)), "{s} should be allowed");
        }
    }

    #[test]
    fn ipv6_loopback_ula_linklocal_and_mapped_are_forbidden() {
        for s in [
            "::1",
            "::",
            "fc00::1",
            "fd12:3456:789a::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
        ] {
            assert!(is_forbidden_ip(ip(s)), "{s} should be forbidden");
        }
    }

    #[test]
    fn public_ipv6_is_allowed() {
        assert!(!is_forbidden_ip(ip("2606:4700:4700::1111")));
    }

    #[test]
    fn validate_url_blocks_unspecified_and_allows_public() {
        assert!(matches!(
            validate_url(
                &Url::parse("http://0.0.0.0:8080/").unwrap(),
                super::PrivateNetworkPolicy::Deny
            ),
            Err(crate::types::NetError::Ssrf(_))
        ));
        assert!(matches!(
            validate_url(
                &Url::parse("http://127.0.0.1/").unwrap(),
                super::PrivateNetworkPolicy::Deny
            ),
            Err(crate::types::NetError::Ssrf(_))
        ));
        assert!(validate_url(
            &Url::parse("http://example.com/").unwrap(),
            super::PrivateNetworkPolicy::Deny
        )
        .is_ok());
        assert!(validate_url(
            &Url::parse("http://127.0.0.1/").unwrap(),
            super::PrivateNetworkPolicy::Allow
        )
        .is_ok());
    }
}
