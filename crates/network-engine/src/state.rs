use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HttpCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: Option<String>,
    pub expires_unix: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HttpStateSnapshot {
    pub version: u64,
    pub current_url: String,
    pub cookies: Vec<HttpCookie>,
    pub cache_validators: BTreeMap<String, String>,
    pub user_agent: String,
    pub language: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ResponseStateDelta {
    pub cookies: Vec<HttpCookie>,
    pub cache_validators: BTreeMap<String, String>,
}
