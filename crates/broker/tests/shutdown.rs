use std::{sync::Arc, time::Duration};

use broker::{serve_listener_graceful, testing::app_with_admin, RejectionWorkerStats};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::Notify,
};

#[tokio::test]
async fn graceful_shutdown_returns_and_refuses_new_connections() {
    let (app, _authority, _bearer) = app_with_admin(4).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let notify = Arc::new(Notify::new());
    let shutdown = {
        let notify = Arc::clone(&notify);
        async move {
            notify.notified().await;
        }
    };
    let server = tokio::spawn(serve_listener_graceful(
        listener,
        app,
        8,
        2,
        RejectionWorkerStats::default(),
        shutdown,
        std::future::pending(),
    ));
    TcpStream::connect(addr)
        .await
        .expect("server accepts before shutdown");
    notify.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server returns within 5s of the shutdown signal");
    result
        .expect("serve task did not panic")
        .expect("serve completed without io error");
    assert!(
        TcpStream::connect(addr).await.is_err(),
        "new connections are refused after graceful shutdown"
    );
}

#[tokio::test]
async fn drain_deadline_forces_return_when_shutdown_stalls() {
    let (app, _authority, _bearer) = app_with_admin(4).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        serve_listener_graceful(
            listener,
            app,
            8,
            2,
            RejectionWorkerStats::default(),
            std::future::pending(),
            async {
                tokio::time::sleep(Duration::from_millis(50)).await;
            },
        ),
    )
    .await
    .expect("serve returns within 5s even though shutdown never completes");
    result.expect("serve completed without io error");
}
