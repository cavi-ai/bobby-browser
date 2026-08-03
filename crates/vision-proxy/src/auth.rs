use axum::http::HeaderMap;
use subtle::ConstantTimeEq;

pub fn authorize(headers: &HeaderMap, expected_token: &str) -> bool {
    let auth = match headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    {
        Some(value) => value,
        None => return false,
    };
    const PREFIX: &str = "Bearer ";
    if !auth.starts_with(PREFIX) {
        return false;
    }
    let token = &auth[PREFIX.len()..];
    token.as_bytes().ct_eq(expected_token.as_bytes()).into()
}
