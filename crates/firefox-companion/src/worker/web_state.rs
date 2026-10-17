//! Cookie reads, writes, and deletion for the Firefox worker.
use super::{driver_error, js_string, now_unix_seconds, FirefoxCompanionWorker};
use async_trait::async_trait;
use serde_json::{json, Value};
use types::{CommandError, ErrorCode, Evidence, PageId};

#[async_trait]
impl worker_pool::WebStateEngine for FirefoxCompanionWorker {
    async fn get_cookies(
        &self,
        page_id: &PageId,
        command: &types::GetCookiesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.context(page_id).await?;
        let response = self.transport.send("storage.getCookies", json!({})).await?;
        let cookies = response
            .get("cookies")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                driver_error(
                    ErrorCode::BrowserCommandFailed,
                    "Firefox cookie read returned no cookie list",
                    false,
                )
            })?;
        let urls: Vec<&str> = command.urls.iter().map(String::as_str).collect();
        let mut records = Vec::new();
        for cookie in cookies {
            let domain = cookie
                .get("domain")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let path = cookie.get("path").and_then(Value::as_str).unwrap_or("/");
            if !urls.is_empty() && !urls.iter().any(|url| cookie_matches_url(domain, path, url)) {
                continue;
            }
            if records.len() == 2048 {
                return Err(driver_error(ErrorCode::InvalidRequest, "Firefox cookie result exceeds the 2048-cookie bound; narrow the requested URLs", false));
            }
            records.push(types::CookieRecord {
                name: cookie
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                value: cookie
                    .pointer("/value/value")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                domain: domain.to_owned(),
                path: path.to_owned(),
                secure: cookie
                    .get("secure")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                http_only: cookie
                    .get("httpOnly")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                same_site: cookie
                    .get("sameSite")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                expires_unix: cookie.get("expiry").and_then(Value::as_f64),
            });
        }
        Ok(vec![Evidence::CookieState {
            page_id: Some(page_id.clone()),
            cookies: records,
        }])
    }

    async fn set_cookies(
        &self,
        page_id: &PageId,
        command: &types::SetCookiesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let context = self.context(page_id).await?;
        if command.cookies.len() > 128 {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                "cookie set exceeds the 128-cookie bound",
                false,
            ));
        }
        if command.cookies.iter().any(|cookie| cookie.http_only) {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                "httpOnly cookies cannot be honored on this Firefox build (storage.setCookies is unsupported); refused rather than downgraded",
                false,
            ));
        }
        for cookie in &command.cookies {
            let mut assignment = format!("{}={}", cookie.name, cookie.value);
            assignment.push_str(&format!("; path={}", cookie.path.as_deref().unwrap_or("/")));
            if cookie.secure {
                assignment.push_str("; secure");
            }
            if let Some(same_site) = &cookie.same_site {
                assignment.push_str(&format!("; samesite={same_site}"));
            }
            if let Some(expires) = cookie.expires_unix {
                let max_age = (expires - now_unix_seconds()).max(0.0) as u64;
                assignment.push_str(&format!("; max-age={max_age}"));
            }
            let statement = format!("document.cookie = {}", js_string(&assignment));
            self.evaluate_page_script(&context, &statement).await?;
        }
        worker_pool::WebStateEngine::get_cookies(
            self,
            page_id,
            &types::GetCookiesCommand {
                urls: command
                    .cookies
                    .iter()
                    .map(|cookie| cookie.url.clone())
                    .collect(),
            },
        )
        .await
    }

    async fn delete_cookies(
        &self,
        page_id: &PageId,
        command: &types::DeleteCookiesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let context = self.context(page_id).await?;
        let current = worker_pool::WebStateEngine::get_cookies(
            self,
            page_id,
            &types::GetCookiesCommand {
                urls: command.urls.clone(),
            },
        )
        .await?;
        let Some(Evidence::CookieState { cookies, .. }) = current.first() else {
            return Ok(current);
        };
        if cookies.iter().any(|cookie| {
            cookie.http_only && (command.names.is_empty() || command.names.contains(&cookie.name))
        }) {
            return Err(driver_error(ErrorCode::InvalidRequest, "Firefox cannot delete HttpOnly cookies through document.cookie; select Chromium for this operation", false));
        }
        for cookie in cookies {
            if !command.names.is_empty() && !command.names.contains(&cookie.name) {
                continue;
            }
            self.evaluate_page_script(
                &context,
                &format!(
                    "document.cookie = {}",
                    js_string(&format!(
                        "{}=; path={}; expires=Thu, 01 Jan 1970 00:00:00 GMT",
                        cookie.name, cookie.path,
                    )),
                ),
            )
            .await?;
        }
        worker_pool::WebStateEngine::get_cookies(
            self,
            page_id,
            &types::GetCookiesCommand {
                urls: command.urls.clone(),
            },
        )
        .await
    }
}

/// Cookie-domain matching is suffix-with-dot-boundary, not substring:
/// `example.com` must match `https://app.example.com/` but never
/// `https://notexample.com/`. The path must be a real path prefix, and a URL
/// that does not parse never matches.
fn cookie_matches_url(domain: &str, path: &str, url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let bare = domain.trim_start_matches('.');
    let host_matches = host == bare || host.ends_with(&format!(".{bare}"));
    host_matches && parsed.path().starts_with(path)
}

#[cfg(test)]
mod cookie_match_tests {
    use super::cookie_matches_url;

    #[test]
    fn cookie_domain_matching_is_dot_bounded_not_substring() {
        assert!(cookie_matches_url(
            "example.com",
            "/",
            "https://example.com/"
        ));
        assert!(cookie_matches_url(
            ".example.com",
            "/",
            "https://app.example.com/"
        ));
        assert!(!cookie_matches_url(
            "example.com",
            "/",
            "https://notexample.com/"
        ));
        assert!(!cookie_matches_url(
            "example.com",
            "/",
            "https://example.com.evil.test/"
        ));
        assert!(cookie_matches_url(
            "example.com",
            "/app",
            "https://example.com/app/x"
        ));
        assert!(!cookie_matches_url(
            "example.com",
            "/app",
            "https://example.com/other"
        ));
        assert!(!cookie_matches_url("example.com", "/", "not a url"));
    }
}
