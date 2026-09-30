//! Exercise the shipped adapter while the host still holds its stdin open.
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

pub async fn owner_close_exits_with_open_stdin(binary: &str) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
        // Let the adapter start its stdin read before closing the owner side.
        tokio::time::sleep(Duration::from_millis(100)).await;
        socket.close(None).await.unwrap();
    });
    let mut child = ChildGuard(
        Command::new(binary)
            .env("BOBBY_RUNTIME_URL", origin)
            .env(
                "AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN",
                "test-credential-with-at-least-32-bytes",
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let _open_input = child.0.stdin.take().unwrap();
    let status = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("adapter hung after owner shutdown with stdin open");
    assert!(status.success());
    server.await.unwrap();
}
