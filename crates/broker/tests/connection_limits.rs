use std::{
    future::Future,
    time::{Duration, Instant},
};

use axum::{routing::get, Router};
use broker::{serve_listener, serve_listener_with_rejection_limit, RejectionWorkerStats};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

async fn request(stream: &mut TcpStream) {
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
}

async fn with_frozen_time<F: Future>(future: F) -> F::Output {
    tokio::time::pause();
    // Paused Tokio time auto-advances when the runtime is idle, including while
    // waiting for native TCP I/O. Keep a task runnable and bound the phase using
    // real time so rejection timers stay frozen without hiding a socket hang.
    let mut wall_timeout = tokio::spawn(async {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            tokio::task::yield_now().await;
        }
    });
    let result = tokio::select! {
        result = future => result,
        _ = &mut wall_timeout => panic!("native socket phase exceeded its wall-clock deadline"),
    };
    wall_timeout.abort();
    let _ = wall_timeout.await;
    tokio::time::resume();
    result
}

#[tokio::test]
async fn overflow_flood_uses_a_bounded_rejection_worker_pool() {
    const ADMITTED: usize = 64;
    const REJECTORS: usize = 16;
    const FLOOD: usize = 256;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new().route("/healthz", get(|| async { "ok" }));
    let stats = RejectionWorkerStats::default();
    let server = tokio::spawn(serve_listener_with_rejection_limit(
        listener,
        app,
        ADMITTED,
        REJECTORS,
        stats.clone(),
    ));

    let mut admitted = Vec::with_capacity(ADMITTED);
    for _ in 0..ADMITTED {
        let mut stream = TcpStream::connect(address).await.unwrap();
        request(&mut stream).await;
        assert!(response_headers(&mut stream)
            .await
            .starts_with(b"HTTP/1.1 200"));
        admitted.push(stream);
    }

    let slow_rejections = with_frozen_time(async {
        let mut slow_rejections = Vec::with_capacity(REJECTORS);
        for _ in 0..REJECTORS {
            slow_rejections.push(TcpStream::connect(address).await.unwrap());
        }
        while stats.active() < REJECTORS {
            tokio::task::yield_now().await;
        }

        // Model a loaded runner being descheduled beyond the rejection idle timeout.
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(stats.active(), REJECTORS);

        let mut flood = Vec::with_capacity(FLOOD);
        for _ in 0..FLOOD {
            flood.push(TcpStream::connect(address).await.unwrap());
        }
        let mut reads = tokio::task::JoinSet::new();
        for mut stream in flood {
            reads.spawn(async move {
                let mut byte = [0_u8; 1];
                stream.read(&mut byte).await.is_ok_and(|count| count == 0)
            });
        }
        let mut promptly_closed = 0;
        while let Some(closed) = reads.join_next().await {
            promptly_closed += usize::from(closed.unwrap());
        }
        assert_eq!(
            promptly_closed, FLOOD,
            "every excess socket must close without spawning work"
        );
        slow_rejections
    })
    .await;

    for mut stream in slow_rejections {
        let response = tokio::time::timeout(Duration::from_secs(1), response_headers(&mut stream))
            .await
            .expect("bounded rejection worker should return typed overload");
        assert!(response.starts_with(b"HTTP/1.1 429"));
    }
    tokio::time::timeout(Duration::from_secs(1), async {
        while stats.active() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("rejection workers must drain after their sockets close");
    assert_eq!(stats.peak(), REJECTORS);

    drop(admitted.pop());
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut retried = TcpStream::connect(address).await.unwrap();
    request(&mut retried).await;
    assert!(response_headers(&mut retried)
        .await
        .starts_with(b"HTTP/1.1 200"));

    let shutdown_rejections = with_frozen_time(async {
        let mut shutdown_rejections = Vec::with_capacity(REJECTORS);
        for _ in 0..REJECTORS {
            shutdown_rejections.push(TcpStream::connect(address).await.unwrap());
        }
        while stats.active() < REJECTORS {
            tokio::task::yield_now().await;
        }
        server.abort();
        shutdown_rejections
    })
    .await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while stats.active() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("rejection worker accounting must drain during shutdown");

    drop(shutdown_rejections);
    drop(retried);
    drop(admitted);
}

async fn response_headers(stream: &mut TcpStream) -> Vec<u8> {
    let mut response = Vec::new();
    let mut chunk = [0_u8; 256];
    while !response.windows(4).any(|window| window == b"\r\n\r\n") {
        let count = stream.read(&mut chunk).await.unwrap();
        assert!(count > 0, "server closed before returning response headers");
        response.extend_from_slice(&chunk[..count]);
    }
    response
}

#[tokio::test]
async fn listener_admits_only_the_configured_number_of_live_connections() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new().route("/healthz", get(|| async { "ok" }));
    let server = tokio::spawn(serve_listener(listener, app, 2));

    let mut first = TcpStream::connect(address).await.unwrap();
    request(&mut first).await;
    let first_response = response_headers(&mut first).await;
    assert!(first_response.starts_with(b"HTTP/1.1 200"));

    let mut second = TcpStream::connect(address).await.unwrap();
    request(&mut second).await;
    let second_response = response_headers(&mut second).await;
    assert!(second_response.starts_with(b"HTTP/1.1 200"));

    let mut rejected = TcpStream::connect(address).await.unwrap();
    request(&mut rejected).await;
    let rejected_response = response_headers(&mut rejected).await;
    assert!(rejected_response.starts_with(b"HTTP/1.1 429"));
    assert!(rejected_response
        .windows(b"retry-after: 1".len())
        .any(|window| window.eq_ignore_ascii_case(b"retry-after: 1")));

    drop(first);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut retried = TcpStream::connect(address).await.unwrap();
    request(&mut retried).await;
    let admitted_response = response_headers(&mut retried).await;
    assert!(admitted_response.starts_with(b"HTTP/1.1 200"));

    drop(second);
    server.abort();
}
