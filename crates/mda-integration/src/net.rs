//! Outbound egress guard (SSRF): every URL the platform fetches on a tenant's
//! behalf — connector `base_url`, webhook subscription `url` — is
//! scheme-checked and resolved, and requests to private/reserved targets are
//! refused. Covers RFC1918, loopback, link-local (which includes the cloud
//! metadata endpoints at `169.254.169.254`), unspecified, and IPv6
//! unique-local/link-local addresses, against every address the host resolves
//! to (a DNS name pointing only at internal IPs is rejected too).
//!
//! On-prem topologies where connectors legitimately target internal hosts can
//! opt out with `MDA_ALLOW_PRIVATE_EGRESS=1` (operator-set; the egress surface
//! itself is admin-gated at the API layer).
//!
//! Residual: a DNS name that rebinds to a private address between the check
//! and the connect (millisecond window, resolved immediately before the
//! request) — pinning resolved IPs into the connection is a follow-up if the
//! threat model demands it.

use mda_core::{Error, Result};

/// Cap on a single fetched response body (connector pulls). Bounds memory for
/// an attacker-controlled or misbehaving external endpoint.
pub const MAX_RESPONSE_BYTES: u64 = 10 * 1024 * 1024;

/// Outbound request timeout (covers connect + headers + body).
pub const EGRESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// TCP/TLS connect timeout.
pub const EGRESS_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Parse an outbound URL and enforce the http/https schemes + a real host.
pub fn parse_outbound_url(raw: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(raw.trim())
        .map_err(|e| Error::Invalid(format!("invalid url {raw:?}: {e}")))?;
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(Error::Invalid(format!(
                "url scheme must be http or https (got {other})"
            )))
        }
    }
    if url.host_str().map(str::trim).unwrap_or("").is_empty() {
        return Err(Error::Invalid("url must have a host".into()));
    }
    Ok(url)
}

fn ip_is_private(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                // 0.0.0.0/8 "this network" and 100.64.0.0/10 carrier-grade NAT
                || v4.octets()[0] == 0
                || v4.octets()[0] == 100 && (v4.octets()[1] & 0b1100_0000) == 0b0100_0000
        }
        std::net::IpAddr::V6(v6) => {
            // segment bitmasks instead of is_unique_local()/is_unicast_link_local()
            // (stable only since 1.84; workspace MSRV is 1.80)
            let s0 = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || (s0 & 0xfe00) == 0xfc00 // fc00::/7 unique-local
                || (s0 & 0xffc0) == 0xfe80 // fe80::/10 link-local
        }
    }
}

/// Resolve the URL's host and reject private/resolved-private targets. Set
/// `MDA_ALLOW_PRIVATE_EGRESS=1` to allow internal egress (on-prem connectors).
pub async fn assert_public_egress(url: &reqwest::Url) -> Result<()> {
    if std::env::var("MDA_ALLOW_PRIVATE_EGRESS").as_deref() == Ok("1") {
        return Ok(());
    }
    let host = url.host_str().unwrap_or_default();
    let addrs: Vec<std::net::IpAddr> = if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        vec![ip]
    } else {
        let port = url.port_or_known_default().unwrap_or(80);
        tokio::net::lookup_host((host, port))
            .await
            .map_err(|e| Error::Invalid(format!("host {host:?} did not resolve: {e}")))?
            .map(|sa| sa.ip())
            .collect()
    };
    if addrs.is_empty() {
        return Err(Error::Invalid(format!("host {host:?} did not resolve")));
    }
    for ip in addrs {
        if ip_is_private(ip) {
            return Err(Error::Invalid(format!(
                "refusing outbound request to private/reserved address {ip} \
                 (SSRF guard; set MDA_ALLOW_PRIVATE_EGRESS=1 to allow internal egress)"
            )));
        }
    }
    Ok(())
}

/// The hardened egress client: total + connect timeouts and no redirect
/// following (a redirect target must not receive the request's auth headers —
/// reqwest only strips the well-known ones, not custom `X-Api-Key` style
/// connector headers).
pub fn egress_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(EGRESS_TIMEOUT)
        .connect_timeout(EGRESS_CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|e| panic!("egress client build failed: {e}"))
}

/// Read a response body capped at [`MAX_RESPONSE_BYTES`]. `Content-Length` is
/// honored when present; chunked streams are truncated at the cap either way.
pub async fn read_capped(mut resp: reqwest::Response) -> Result<Vec<u8>> {
    if let Some(len) = resp.content_length() {
        if len > MAX_RESPONSE_BYTES {
            return Err(Error::Invalid(format!(
                "response exceeds the {MAX_RESPONSE_BYTES}-byte egress cap"
            )));
        }
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(Error::internal)? {
        buf.extend_from_slice(&chunk);
        if buf.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(Error::Invalid(format!(
                "response exceeds the {MAX_RESPONSE_BYTES}-byte egress cap"
            )));
        }
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_http_schemes() {
        assert!(parse_outbound_url("file:///etc/passwd").is_err());
        assert!(parse_outbound_url("gopher://x").is_err());
        assert!(parse_outbound_url("not a url").is_err());
        assert!(parse_outbound_url("http://").is_err());
        assert!(parse_outbound_url("https://api.example.com/x").is_ok());
        assert!(parse_outbound_url("http://api.example.com").is_ok());
    }

    #[test]
    fn private_ips_detected() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
        ] {
            let ip: std::net::IpAddr = ip.parse().unwrap();
            assert!(ip_is_private(ip), "{ip} must be private");
        }
        for ip in ["1.1.1.1", "93.184.216.34", "2606:4700:4700::1111"] {
            let ip: std::net::IpAddr = ip.parse().unwrap();
            assert!(!ip_is_private(ip), "{ip} must be public");
        }
    }

    #[tokio::test]
    async fn loopback_literal_rejected_unless_opted_out() {
        let url = parse_outbound_url("http://127.0.0.1:8080/hook").unwrap();
        assert!(assert_public_egress(&url).await.is_err());
        // opt-out is read per call, so a test-set env var flips it
        std::env::set_var("MDA_ALLOW_PRIVATE_EGRESS", "1");
        assert!(assert_public_egress(&url).await.is_ok());
        std::env::remove_var("MDA_ALLOW_PRIVATE_EGRESS");
        assert!(assert_public_egress(&url).await.is_err());
    }
}
