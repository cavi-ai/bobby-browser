use std::collections::BTreeMap;

#[derive(Clone)]
pub struct HttpCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: Option<String>,
    pub expires_unix: Option<f64>,
    pub priority: Option<String>,
    pub source_scheme: Option<String>,
    pub source_port: Option<i64>,
    pub partition_key: Option<HttpCookiePartitionKey>,
}

#[derive(Clone)]
pub struct HttpCookiePartitionKey {
    pub top_level_site: String,
    pub has_cross_site_ancestor: bool,
}

pub struct HttpStateSnapshot {
    pub version: u64,
    pub current_url: String,
    pub cookies: Vec<HttpCookie>,
    pub cache_validators: BTreeMap<String, String>,
    pub user_agent: String,
    pub language: String,
}

#[derive(Default)]
pub struct ResponseStateDelta {
    pub cookies: Vec<HttpCookie>,
    pub cache_validators: BTreeMap<String, String>,
}
