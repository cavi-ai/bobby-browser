use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn vision_status_distinguishes_an_unrelated_listener() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut request = [0_u8; 1024];
                    let _ = stream.read(&mut request).unwrap();
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
                        )
                        .unwrap();
                    return true;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept failed: {error}"),
            }
        }
        false
    });
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"[vision]
provider = "lmstudio"
endpoint_url = "http://{address}/vision"

[vision.providers.lmstudio]
base_url = "http://127.0.0.1:1234/v1"
model = "local-model"
"#
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_bobby"))
        .args(["vision", "status", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        server.join().unwrap(),
        "vision status never probed the endpoint"
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("occupied by an unknown service"),
        "{stdout}"
    );
    assert!(!stdout.contains("vision-service: running"), "{stdout}");
}
