//! MCP tool-call and serialized JSON response-byte limits for fixture journeys.
//! Counts include error replies and the full JSON-RPC result, including text and
//! structured content. Handshakes, notifications, and line delimiters are excluded.
//! Measurements are bytes, not token estimates or latency guarantees.
use std::sync::Mutex;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub calls: u64,
    pub response_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct Budget {
    pub calls: u64,
    pub response_bytes: u64,
}

impl Usage {
    pub fn since(self, earlier: Self) -> Result<Self, String> {
        Ok(Self {
            calls: self
                .calls
                .checked_sub(earlier.calls)
                .ok_or("tool-call counter regressed")?,
            response_bytes: self
                .response_bytes
                .checked_sub(earlier.response_bytes)
                .ok_or("response-byte counter regressed")?,
        })
    }

    pub fn check(self, budget: Option<Budget>) -> Result<(), String> {
        let budget = budget.ok_or("no journey budget declared")?;
        if self.calls == 0 || self.response_bytes == 0 {
            return Err("journey measurements are missing".into());
        }
        if self.calls > budget.calls {
            return Err(format!("tool calls {} exceed {}", self.calls, budget.calls));
        }
        if self.response_bytes > budget.response_bytes {
            return Err(format!(
                "response bytes {} exceed {}",
                self.response_bytes, budget.response_bytes
            ));
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct Meter(Mutex<Usage>);

impl Meter {
    pub fn record(&self, response: &serde_json::Value) {
        let bytes = serde_json::to_vec(response)
            .expect("serialize MCP response")
            .len() as u64;
        let mut usage = self.0.lock().expect("journey meter lock");
        usage.calls = usage
            .calls
            .checked_add(1)
            .expect("tool-call counter overflow");
        usage.response_bytes = usage
            .response_bytes
            .checked_add(bytes)
            .expect("response-byte counter overflow");
    }

    pub fn snapshot(&self) -> Usage {
        *self.0.lock().expect("journey meter lock")
    }
}

/// Call ceilings stay fixed; byte ceilings allow bounded serialization variation.
/// Unknown engine/journey pairs have no fallback allowance.
pub fn limit(engine: &str, journey: &str) -> Option<Budget> {
    let (calls, response_bytes) = match (engine, journey) {
        ("chromium", "j1_sign_in_redirect_then_app") => (8, 16384),
        ("chromium", "j2_search_and_follow_result") => (10, 32768),
        ("chromium", "j3_form_validation_then_success") => (7, 28672),
        ("chromium", "j4_upload_hidden_and_visible") => (6, 16384),
        ("chromium", "j5_popup_and_frame") => (11, 28672),
        ("chromium", "j6_cross_site_navigation_recovery") => (13, 28672),
        ("firefox", "j1_sign_in_redirect_then_app") => (8, 20480),
        ("firefox", "j2_search_and_follow_result") => (10, 32768),
        ("firefox", "j3_form_validation_then_success") => (7, 28672),
        ("firefox", "j4_upload_hidden_and_visible") => (6, 16384),
        ("firefox", "j5_popup_and_frame") => (11, 28672),
        ("firefox", "j6_cross_site_navigation_recovery") => (13, 32768),
        ("firefox", "j7_native_host_killed_mid_session") => (7, 16384),
        _ => return None,
    };
    Some(Budget {
        calls,
        response_bytes,
    })
}
