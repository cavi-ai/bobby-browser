//! Site-key derivation: scheme + registrable domain (eTLD+1), never a full
//! URL. Subdomains collapse to the registrable domain; ports, userinfo,
//! query, and fragment never reach the key. IP literals and single-label
//! hosts key as-is.
//!
//! Registrable-domain boundaries come from the maintained Public Suffix List,
//! including its private section. That keeps unrelated hosted tenants in
//! separate context records.

/// Derives the persisted site key for a page URL.
///
/// Returns `None` for non-hierarchical URLs (`about:blank`, `data:`) and for
/// URLs without a host — there is no site identity to key context by.
pub fn site_key(page_url: &str) -> Option<String> {
    let parsed = url::Url::parse(page_url).ok()?;
    let scheme = parsed.scheme().to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    let registrable = registrable_domain(&host);
    Some(format!("{scheme}://{registrable}"))
}

fn registrable_domain(host: &str) -> String {
    if host.parse::<std::net::IpAddr>().is_ok()
        || (host.starts_with('[') && host.ends_with(']'))
        || !host.contains('.')
    {
        return host.to_string();
    }
    psl::domain(host.as_bytes())
        .and_then(|domain| std::str::from_utf8(domain.as_bytes()).ok())
        .unwrap_or(host)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::site_key;

    #[test]
    fn site_key_table() {
        let cases: &[(&str, Option<&str>)] = &[
            (
                "https://example.com/path?q=1#frag",
                Some("https://example.com"),
            ),
            ("https://app.example.com/a", Some("https://example.com")),
            ("https://deep.app.example.com/", Some("https://example.com")),
            ("http://example.com/", Some("http://example.com")),
            ("https://example.com:8443/", Some("https://example.com")),
            ("https://user:pw@example.com/", Some("https://example.com")),
            ("https://example.co.uk/", Some("https://example.co.uk")),
            ("https://shop.example.co.uk/", Some("https://example.co.uk")),
            ("https://example.com.au/x", Some("https://example.com.au")),
            ("https://alice.github.io/app", Some("https://alice.github.io")),
            ("https://bob.github.io/app", Some("https://bob.github.io")),
            ("https://one.pages.dev/", Some("https://one.pages.dev")),
            ("https://two.pages.dev/", Some("https://two.pages.dev")),
            ("https://127.0.0.1:3000/app", Some("https://127.0.0.1")),
            ("http://[::1]:8080/x", Some("http://[::1]")),
            ("http://localhost:9000/", Some("http://localhost")),
            (
                "https://xn--nxasmq6b.example.se/",
                Some("https://example.se"),
            ),
            ("about:blank", None),
            ("data:text/html,<p>hi</p>", None),
            ("not a url", None),
        ];
        for (input, expected) in cases {
            assert_eq!(site_key(input).as_deref(), *expected, "input: {input}");
        }
    }
}
