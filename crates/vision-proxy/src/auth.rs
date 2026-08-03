use axum::http::HeaderMap;
use subtle::ConstantTimeEq;

pub fn authorize(headers: &HeaderMap, expected_token: &str) -> Result<(), ()> {
    let auth = match headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    {
        Some(value) => value,
        None => return Err(()),
    };
    const PREFIX: &str = "Bearer ";
    if !auth.starts_with(PREFIX) {
        return Err(());
    }
    let token = &auth[PREFIX.len()..];
    if token.as_bytes().ct_eq(expected_token.as_bytes()).into() {
        Ok(())
    } else {
        Err(())
    }
}
