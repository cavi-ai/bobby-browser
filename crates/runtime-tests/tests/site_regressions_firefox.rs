//! Browser behavior cases on minimal local pages against the installed
//! Firefox companion. One Firefox runs every case in sequence; each failing
//! case is named in the final panic. See `support/cases.rs`.

mod support;

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;

use futures_util::FutureExt;
use support::cases;
use support::rig::Rig;

type Case = for<'a> fn(&'a Rig) -> Pin<Box<dyn Future<Output = ()> + 'a>>;

/// The cases are a table of boxed futures walked by one loop. Awaiting each
/// case inline made this test's debug-build poll frame grow with every case
/// and overflowed the 2 MB test-thread stack before the first case finished.
macro_rules! case_table {
    ($($case:ident),+ $(,)?) => {
        [$((stringify!($case), (|rig| Box::pin(cases::$case(rig))) as Case)),+]
    };
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed headed Firefox and paired test profile"]
async fn site_regressions_hold_on_firefox() {
    let rig = Rig::firefox().await;
    let mut failures: Vec<&str> = Vec::new();
    // The candidate cap and the control walk these extra cases exercise exist only in the Firefox companion.
    let table = cases::every_case!(
        case_table,
        oversized_page_reports_truncation_not_target_not_found,
        typing_behind_a_modal_reports_the_dialog
    );
    for (name, case) in table {
        if AssertUnwindSafe(case(&rig)).catch_unwind().await.is_err() {
            failures.push(name);
        }
    }
    assert!(failures.is_empty(), "failing cases: {failures:?}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed headed Firefox and paired test profile"]
async fn snapshot_scoped_behind_a_modal_reports_the_dialog() {
    let rig = Rig::firefox().await;
    cases::snapshot_scoped_behind_a_modal_reports_the_dialog(&rig).await;
}
