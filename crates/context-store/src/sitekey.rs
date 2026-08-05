//! Site-key derivation: scheme + registrable domain (eTLD+1), never a full
//! URL. Subdomains collapse to the registrable domain; ports, userinfo,
//! query, and fragment never reach the key. IP literals and single-label
//! hosts key as-is.
//!
//! The workspace carries no public-suffix dependency (verified against
//! `Cargo.lock` 2026-08-05), so the multi-label suffix table below is
//! hand-rolled and deliberately short: it covers the common second-level
//! registries and defaults to last-two-labels otherwise. A wrong entry only
//! over- or under-shards a site's file; it never leaks one site's context
//! into a different registrable domain.

/// Well-known multi-label public suffixes. Keep sorted; lookup is linear
/// over a table this small.
const MULTI_LABEL_SUFFIXES: &[&str] = &[
    "ac.jp", "ac.kr", "ac.nz", "ac.th", "ac.uk", "co.id", "co.il", "co.in", "co.jp", "co.kr",
    "co.nz", "co.th", "co.uk", "co.za", "com.ar", "com.au", "com.br", "com.cn", "com.co",
    "com.hk", "com.mx", "com.my", "com.ph", "com.pl", "com.sa", "com.sg", "com.tr", "com.tw",
    "com.vn", "edu.au", "edu.cn", "edu.sg", "firm.in", "gov.au", "gov.cn", "gov.in", "gov.uk",
    "ne.jp", "net.au", "net.cn", "net.in", "net.uk", "or.jp", "or.kr", "org.au", "org.cn",
    "org.in", "org.nz", "org.uk", "org.za",
];

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
    let labels: Vec<&str> = host.split('.').collect();
    for suffix in MULTI_LABEL_SUFFIXES {
        if host.ends_with(suffix) && labels.len() > 2 {
            let suffix_labels = suffix.split('.').count();
            let keep = (suffix_labels + 1).min(labels.len());
            return labels[labels.len() - keep..].join(".");
        }
    }
    labels[labels.len() - 2..].join(".")
}

#[cfg(test)]
mod tests {
    use super::site_key;

    #[test]
    fn site_key_table() {
        let cases: &[(&str, Option<&str>)] = &[
            ("https://example.com/path?q=1#frag", Some("https://example.com")),
            ("https://app.example.com/a", Some("https://example.com")),
            ("https://deep.app.example.com/", Some("https://example.com")),
            ("http://example.com/", Some("http://example.com")),
            ("https://example.com:8443/", Some("https://example.com")),
            ("https://user:pw@example.com/", Some("https://example.com")),
            ("https://example.co.uk/", Some("https://example.co.uk")),
            ("https://shop.example.co.uk/", Some("https://example.co.uk")),
            ("https://example.com.au/x", Some("https://example.com.au")),
            ("https://127.0.0.1:3000/app", Some("https://127.0.0.1")),
            ("http://[::1]:8080/x", Some("http://[::1]")),
            ("http://localhost:9000/", Some("http://localhost")),
            ("https://xn--nxasmq6b.example.se/", Some("https://example.se")),
            ("about:blank", None),
            ("data:text/html,<p>hi</p>", None),
            ("not a url", None),
        ];
        for (input, expected) in cases {
            assert_eq!(site_key(input).as_deref(), *expected, "input: {input}");
        }
    }
}
