use std::sync::Arc;

use broker::{router, AppState};
use chrono::{Duration, Utc};
use interface_core::{AuthorityStore, RuntimeInterface};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use types::{Capability, PrincipalId};

#[tokio::main]
async fn main() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let authority = AuthorityStore::in_memory();
    let expires_at = Utc::now() + Duration::minutes(5);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [Capability::SessionRead, Capability::SessionWrite],
            expires_at,
        )
        .await
        .unwrap()
        .expose_once();
    let denied_token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [Capability::SessionRead],
            expires_at,
        )
        .await
        .unwrap()
        .expose_once();
    let runtime = RuntimeService::default();
    let app = router(AppState::new(
        Arc::new(authority),
        move |handle| {
            Arc::new(AuthenticatedRuntime::new(runtime.clone(), handle))
                as Arc<dyn RuntimeInterface>
        },
        config::InterfaceConfig::default(),
    ));
    println!(
        "{}",
        serde_json::json!({"endpoint":format!("http://{address}"),"token":token,"deniedToken":denied_token})
    );
    axum::serve(listener, app).await.unwrap();
}
