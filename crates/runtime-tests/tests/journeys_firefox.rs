//! End-to-end agent journeys against the installed Firefox companion. One
//! Firefox runs every journey in sequence; each failing journey is named in
//! the final panic. See `support/journeys.rs`.

mod support;

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;

use futures_util::FutureExt;
use support::journeys::{self, Dirs};
use support::rig::Rig;

type Journey = for<'a> fn(&'a Rig, &'a Dirs) -> Pin<Box<dyn Future<Output = ()> + 'a>>;

/// The journeys are a table of boxed futures walked by one loop, as in
/// `site_regressions_firefox.rs`, to keep the debug-build poll frame small.
macro_rules! journey_table {
    ($($journey:ident),+ $(,)?) => {
        [$((stringify!($journey), (|rig, dirs| Box::pin(journeys::$journey(rig, dirs))) as Journey)),+]
    };
}

#[tokio::test]
#[ignore = "requires installed Firefox and paired test profile"]
async fn journeys_hold_on_firefox() {
    let dirs = Dirs::new();
    let rig = Rig::firefox_with_upload_roots(dirs.upload_roots()).await;
    let mut failures: Vec<&str> = Vec::new();
    let table = journey_table!(
        j1_sign_in_redirect_then_app,
        j2_search_and_follow_result,
        j3_form_validation_then_success,
        j4_upload_hidden_and_visible,
        j5_popup_and_frame,
        j6_cross_site_navigation_recovery,
        j7_native_host_killed_mid_session,
    );
    for (name, journey) in table {
        if AssertUnwindSafe(journey(&rig, &dirs))
            .catch_unwind()
            .await
            .is_err()
        {
            failures.push(name);
        }
    }
    assert!(failures.is_empty(), "failing journeys: {failures:?}");
}
