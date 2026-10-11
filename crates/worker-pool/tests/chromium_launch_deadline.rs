#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use chromiumoxide::{Browser, BrowserConfig};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

#[tokio::test]
async fn launch_deadline_includes_a_stalled_devtools_handshake() {
    let root = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let executable = root.path().join("fake-chrome");
    let pid_file = root.path().join("child.pid");
    std::fs::write(
        &executable,
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nprintf 'DevTools listening on ws://{address}/devtools/browser/test\\n' >&2\nexec sleep 60\n",
            pid_file.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (request_seen, mut observed_request) = tokio::sync::oneshot::channel();
    let peer = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = BufReader::new(socket);
        let mut line = String::new();
        assert!(socket.read_line(&mut line).await.unwrap() > 0);
        assert_eq!(line, "GET /devtools/browser/test HTTP/1.1\r\n");
        let mut header_bytes = line.len();
        loop {
            line.clear();
            let received = socket.read_line(&mut line).await.unwrap();
            assert!(received > 0);
            header_bytes += received;
            assert!(header_bytes <= 4096);
            if line == "\r\n" {
                break;
            }
        }
        request_seen.send(()).unwrap();
        let mut request = [0; 1];
        // Accept TCP and consume the HTTP upgrade, but never send a handshake reply.
        assert_eq!(socket.read(&mut request).await.unwrap(), 0);
    });
    let config = BrowserConfig::builder()
        .chrome_executable(executable)
        // Leave time for child startup to reach the handshake being tested,
        // while keeping the stalled upgrade's deadline bounded.
        .launch_timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), Browser::launch(config)).await;
    let error = result
        .expect("launch must enforce its deadline while connecting to DevTools")
        .expect_err("the stalled handshake cannot launch a browser");
    assert!(matches!(
        error,
        chromiumoxide::error::CdpError::LaunchTimeout(_)
    ));
    observed_request
        .try_recv()
        .expect("the launch deadline must be exercised during the HTTP upgrade");
    tokio::time::timeout(Duration::from_secs(1), peer)
        .await
        .expect("the failed handshake connection must close")
        .unwrap();
    let pid: i32 = std::fs::read_to_string(pid_file).unwrap().parse().unwrap();
    // Browser::launch owns this child and must kill and reap it on failure.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}
