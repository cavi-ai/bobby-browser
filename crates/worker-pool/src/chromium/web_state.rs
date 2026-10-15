//! Chromium cookie commands and versioned HTTP state synchronization.

use std::collections::BTreeMap;

use async_trait::async_trait;
use chromiumoxide::cdp::browser_protocol::network::{
    Cookie, CookieParam, CookiePartitionKey, CookiePriority, CookieSameSite, CookieSourceScheme,
    SetCookiesParams, TimeSinceEpoch,
};
use network_engine::state::{
    HttpCookie, HttpCookiePartitionKey, HttpStateSnapshot, ResponseStateDelta,
};
use types::{CommandError, ErrorCode, ErrorLayer, Evidence, PageId};

use super::{bounded_cdp, command_failed, driver_error, ChromiumWorker};

#[derive(Default)]
pub(super) struct HttpBridgeState {
    version: u64,
    cache_validators: BTreeMap<String, String>,
}

#[async_trait]
impl crate::WebStateEngine for ChromiumWorker {
    async fn get_cookies(
        &self,
        page_id: &PageId,
        command: &types::GetCookiesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let page = self.page_handle(page_id).await?;
        let mut params = chromiumoxide::cdp::browser_protocol::network::GetCookiesParams::default();
        if !command.urls.is_empty() {
            params.urls = Some(command.urls.clone());
        }
        let result = bounded_cdp(page.execute(params), command_failed)
            .await?
            .result;
        Ok(vec![Evidence::CookieState {
            page_id: Some(page_id.clone()),
            cookies: result.cookies.into_iter().map(cookie_record).collect(),
        }])
    }

    async fn set_cookies(
        &self,
        page_id: &PageId,
        command: &types::SetCookiesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        // Clone the page handle and drop the pages guard before any browser
        // I/O: the read-back below locks the same (non-reentrant) mutex, so
        // holding it here deadlocks every cookie_set.
        let page = self.page_handle(page_id).await?;
        if command.cookies.len() > 128 {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                "cookie set exceeds the 128-cookie bound",
            ));
        }
        let params = chromiumoxide::cdp::browser_protocol::network::SetCookiesParams {
            cookies: command.cookies.iter().map(set_cookie_param).collect(),
        };
        bounded_cdp(page.execute(params), command_failed).await?;
        crate::WebStateEngine::get_cookies(
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
        // Same deadlock avoidance as set_cookies: take the handle, drop the
        // guard, then do I/O (both get_cookies calls lock pages again).
        let page = self.page_handle(page_id).await?;
        let current = crate::WebStateEngine::get_cookies(
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
        for cookie in cookies {
            if !command.names.is_empty() && !command.names.contains(&cookie.name) {
                continue;
            }
            let mut params =
                chromiumoxide::cdp::browser_protocol::network::DeleteCookiesParams::new(
                    &cookie.name,
                );
            params.url = command.urls.first().cloned().or_else(|| {
                Some(format!(
                    "https://{}{}",
                    cookie.domain.trim_start_matches('.'),
                    cookie.path
                ))
            });
            params.domain = Some(cookie.domain.clone());
            params.path = Some(cookie.path.clone());
            bounded_cdp(page.execute(params), command_failed).await?;
        }
        crate::WebStateEngine::get_cookies(
            self,
            page_id,
            &types::GetCookiesCommand {
                urls: command.urls.clone(),
            },
        )
        .await
    }

    fn supports_http_state(&self) -> bool {
        true
    }

    async fn http_state(&self, page_id: &PageId) -> Result<HttpStateSnapshot, CommandError> {
        let page = self.page_handle(page_id).await?;
        let state = self.http_state.lock().await;
        let current_url = page
            .url()
            .await
            .map_err(command_failed)?
            .unwrap_or_default();
        let cookies = page
            .get_cookies()
            .await
            .map_err(command_failed)?
            .into_iter()
            .map(snapshot_cookie)
            .collect::<Result<Vec<_>, _>>()?;
        let user_agent = page
            .evaluate("navigator.userAgent")
            .await
            .map_err(command_failed)?
            .into_value()
            .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
        let language = page
            .evaluate("navigator.language")
            .await
            .map_err(command_failed)?
            .into_value()
            .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error))?;
        Ok(HttpStateSnapshot {
            version: state.version,
            current_url,
            cookies,
            cache_validators: state.cache_validators.clone(),
            user_agent,
            language,
        })
    }

    async fn commit_http_state(
        &self,
        page_id: &PageId,
        expected_version: u64,
        delta: ResponseStateDelta,
    ) -> Result<(), CommandError> {
        let page = self.page_handle(page_id).await?;
        let current_url = bounded_cdp(page.url(), command_failed)
            .await?
            .ok_or_else(|| driver_error(ErrorCode::InvalidRequest, "page URL is unavailable"))?;
        let parsed_url = url::Url::parse(&current_url)
            .map_err(|error| driver_error(ErrorCode::InvalidRequest, error))?;
        validate_state_delta(&delta, &parsed_url)?;
        let mut state = self.http_state.lock().await;
        if state.version != expected_version {
            return Err(http_state_conflict(expected_version, state.version));
        }
        let next_version = state.version.checked_add(1).ok_or_else(|| {
            driver_error(
                ErrorCode::BrowserCommandFailed,
                "HTTP state version exhausted",
            )
        })?;
        if delta.cookies.is_empty() {
            finish_state_commit(&mut state, next_version, delta.cache_validators);
            return Ok(());
        }
        let cookies = delta
            .cookies
            .into_iter()
            .map(|cookie| cookie_param(cookie, &current_url))
            .collect::<Result<Vec<_>, _>>()?;
        apply_state_commit(&mut state, next_version, delta.cache_validators, async {
            page.execute(SetCookiesParams::new(cookies))
                .await
                .map(|_| ())
        })
        .await
        .map_err(command_failed)?;
        Ok(())
    }
}

fn validate_state_delta(
    delta: &ResponseStateDelta,
    current_url: &url::Url,
) -> Result<(), CommandError> {
    let host = current_url.host_str().ok_or_else(|| {
        driver_error(
            ErrorCode::InvalidRequest,
            "page URL does not have a cookie host",
        )
    })?;
    if !matches!(current_url.scheme(), "http" | "https") {
        return Err(driver_error(
            ErrorCode::InvalidRequest,
            "page URL scheme cannot carry HTTP cookies",
        ));
    }
    for cookie in &delta.cookies {
        if cookie.name.is_empty()
            || cookie
                .name
                .bytes()
                .any(|byte| byte <= 0x20 || byte >= 0x7f || b"()<>@,;:\\\"/[]?={}".contains(&byte))
            || cookie.value.contains(['\r', '\n', '\0'])
            || cookie.path.is_empty()
            || !cookie.path.starts_with('/')
        {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                "invalid HTTP cookie",
            ));
        }
        let domain = cookie.domain.strip_prefix('.').unwrap_or(&cookie.domain);
        if !domain.is_empty() {
            let parsed_domain = url::Host::parse(domain)
                .map_err(|_| driver_error(ErrorCode::InvalidRequest, "cookie domain is invalid"))?;
            let normalized_domain = parsed_domain.to_string();
            let in_scope = if matches!(parsed_domain, url::Host::Domain(_)) {
                host.eq_ignore_ascii_case(&normalized_domain)
                    || host
                        .to_ascii_lowercase()
                        .strip_suffix(&normalized_domain)
                        .is_some_and(|prefix| prefix.ends_with('.'))
            } else {
                host.eq_ignore_ascii_case(&normalized_domain)
            };
            if !in_scope {
                return Err(driver_error(
                    ErrorCode::InvalidRequest,
                    "cookie domain is outside the current page scope",
                ));
            }
        }
        if cookie.secure && current_url.scheme() != "https" {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                "secure cookie cannot be set from a non-HTTPS page",
            ));
        }
        if cookie
            .same_site
            .as_deref()
            .is_some_and(|same_site| same_site.eq_ignore_ascii_case("none"))
            && !cookie.secure
        {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                "SameSite=None cookie must be secure",
            ));
        }
        if cookie
            .expires_unix
            .is_some_and(|expiry| !expiry.is_finite() || expiry <= 0.0)
        {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                "cookie expiry must be a finite positive Unix timestamp",
            ));
        }
        let expected_source_scheme = if current_url.scheme() == "https" {
            "Secure"
        } else {
            "NonSecure"
        };
        if cookie
            .source_scheme
            .as_deref()
            .is_some_and(|scheme| !scheme.eq_ignore_ascii_case(expected_source_scheme))
        {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                "cookie source scheme does not match the current page",
            ));
        }
        if cookie.source_port.is_some_and(|port| {
            port == -1
                || !(1..=65_535).contains(&port)
                || current_url.port_or_known_default().map(i64::from) != Some(port)
        }) {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                "cookie source port does not match the current page",
            ));
        }
        if let Some(key) = &cookie.partition_key {
            let site = url::Url::parse(&key.top_level_site).map_err(|_| {
                driver_error(
                    ErrorCode::InvalidRequest,
                    "cookie partition site is invalid",
                )
            })?;
            if !matches!(site.scheme(), "http" | "https") || site.host_str().is_none() {
                return Err(driver_error(
                    ErrorCode::InvalidRequest,
                    "cookie partition site is invalid",
                ));
            }
        }
    }
    Ok(())
}

fn snapshot_cookie(cookie: Cookie) -> Result<HttpCookie, CommandError> {
    if cookie.partition_key_opaque == Some(true) {
        return Err(http_equivalence_unproven(
            "opaque partitioned cookie cannot be represented",
        ));
    }
    if !cookie.session && !cookie.expires.is_finite() {
        return Err(http_equivalence_unproven(
            "cookie expiry cannot be represented",
        ));
    }
    let host_only = !cookie.domain.starts_with('.');
    Ok(HttpCookie {
        name: cookie.name,
        value: cookie.value,
        domain: cookie.domain,
        host_only,
        path: cookie.path,
        secure: cookie.secure,
        http_only: cookie.http_only,
        same_site: cookie.same_site.map(|value| value.as_ref().to_owned()),
        expires_unix: (!cookie.session).then_some(cookie.expires),
        priority: Some(cookie.priority.as_ref().to_owned()),
        source_scheme: Some(cookie.source_scheme.as_ref().to_owned()),
        source_port: Some(cookie.source_port),
        partition_key: cookie.partition_key.map(|key| HttpCookiePartitionKey {
            top_level_site: key.top_level_site,
            has_cross_site_ancestor: key.has_cross_site_ancestor,
        }),
    })
}

fn cookie_param(cookie: HttpCookie, current_url: &str) -> Result<CookieParam, CommandError> {
    let same_site = cookie
        .same_site
        .map(|value| value.parse::<CookieSameSite>())
        .transpose()
        .map_err(|_| driver_error(ErrorCode::InvalidRequest, "invalid cookie SameSite value"))?;
    let mut param = CookieParam::new(cookie.name, cookie.value);
    param.url = Some(current_url.to_owned());
    param.domain = (!cookie.host_only && !cookie.domain.is_empty()).then_some(cookie.domain);
    param.path = (!cookie.path.is_empty()).then_some(cookie.path);
    param.secure = Some(cookie.secure);
    param.http_only = Some(cookie.http_only);
    param.same_site = same_site;
    param.expires = cookie.expires_unix.map(TimeSinceEpoch::new);
    param.priority = cookie
        .priority
        .map(|value| value.parse::<CookiePriority>())
        .transpose()
        .map_err(|_| driver_error(ErrorCode::InvalidRequest, "invalid cookie priority"))?;
    param.source_scheme = cookie
        .source_scheme
        .map(|value| value.parse::<CookieSourceScheme>())
        .transpose()
        .map_err(|_| driver_error(ErrorCode::InvalidRequest, "invalid cookie source scheme"))?;
    param.source_port = cookie.source_port;
    param.partition_key = cookie
        .partition_key
        .map(|key| CookiePartitionKey::new(key.top_level_site, key.has_cross_site_ancestor));
    Ok(param)
}

fn finish_state_commit(
    state: &mut HttpBridgeState,
    next_version: u64,
    validators: BTreeMap<String, String>,
) {
    state.cache_validators.extend(validators);
    state.version = next_version;
}

async fn apply_state_commit<E>(
    state: &mut HttpBridgeState,
    next_version: u64,
    validators: BTreeMap<String, String>,
    application: impl std::future::Future<Output = Result<(), E>>,
) -> Result<(), E> {
    application.await?;
    finish_state_commit(state, next_version, validators);
    Ok(())
}

fn http_state_conflict(expected: u64, actual: u64) -> CommandError {
    CommandError {
        code: ErrorCode::HttpStateConflict,
        message: format!("HTTP state version conflict: expected {expected}, current {actual}"),
        layer: ErrorLayer::Driver,
        retryable: false,
    }
}

fn http_equivalence_unproven(message: impl Into<String>) -> CommandError {
    CommandError {
        code: ErrorCode::HttpEquivalenceUnproven,
        message: message.into(),
        layer: ErrorLayer::Driver,
        retryable: false,
    }
}

fn cookie_record(
    cookie: chromiumoxide::cdp::browser_protocol::network::Cookie,
) -> types::CookieRecord {
    types::CookieRecord {
        name: cookie.name,
        value: cookie.value,
        domain: cookie.domain,
        path: cookie.path,
        secure: cookie.secure,
        http_only: cookie.http_only,
        same_site: cookie.same_site.map(|value| format!("{value:?}")),
        expires_unix: Some(cookie.expires),
    }
}

fn set_cookie_param(
    param: &types::SetCookieParam,
) -> chromiumoxide::cdp::browser_protocol::network::CookieParam {
    use chromiumoxide::cdp::browser_protocol::network::{
        CookieParam, CookieSameSite, TimeSinceEpoch,
    };
    let mut built = CookieParam::new(&param.name, &param.value);
    built.url = Some(param.url.clone());
    built.path = param.path.clone();
    built.secure = Some(param.secure);
    built.http_only = Some(param.http_only);
    built.same_site = param.same_site.as_deref().and_then(|value| match value {
        "Strict" | "strict" => Some(CookieSameSite::Strict),
        "Lax" | "lax" => Some(CookieSameSite::Lax),
        "None" | "none" => Some(CookieSameSite::None),
        _ => None,
    });
    built.expires = param.expires_unix.map(TimeSinceEpoch::new);
    built
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failed_cookie_application_does_not_advance_state() {
        let mut state = HttpBridgeState {
            version: 7,
            cache_validators: BTreeMap::from([("etag".into(), "old".into())]),
        };
        let result = apply_state_commit(
            &mut state,
            8,
            BTreeMap::from([("etag".into(), "new".into())]),
            async { Err::<(), _>("CDP rejected cookies") },
        )
        .await;

        assert_eq!(result.unwrap_err(), "CDP rejected cookies");
        assert_eq!(state.version, 7);
        assert_eq!(state.cache_validators.get("etag").unwrap(), "old");
    }

    #[test]
    fn opaque_partitioned_cookie_fails_closed() {
        let cookie = Cookie {
            name: "partitioned".into(),
            value: "secret".into(),
            domain: "example.test".into(),
            path: "/".into(),
            expires: -1.0,
            size: 17,
            http_only: true,
            secure: true,
            session: true,
            same_site: None,
            priority: CookiePriority::Medium,
            source_scheme: CookieSourceScheme::Secure,
            source_port: 443,
            partition_key: None,
            partition_key_opaque: Some(true),
        };

        let error = match snapshot_cookie(cookie) {
            Ok(_) => panic!("opaque partitioned cookie must fail closed"),
            Err(error) => error,
        };
        assert_eq!(error.code, ErrorCode::HttpEquivalenceUnproven);
    }
}
