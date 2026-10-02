//! A runtime's vision proxy: it reports the port the OS picked and exits when
//! the runtime that started it goes away, so no proxy outlives its runtime and
//! holds a port the next one needs.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn a_managed_vision_proxy_announces_its_port_and_exits_with_its_parent() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_bobby"))
        .args([
            "vision-proxy",
            "--managed",
            "--bind",
            "127.0.0.1:0",
            "--upstream",
            "ollama",
            "--model",
            "llava:7b",
            "--vision-base-url",
            "http://127.0.0.1:9/v1",
        ])
        .env(
            "BOBBY_VISION_TOKEN",
            "vision-proxy-test-token-0123456789abcdef",
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let address: std::net::SocketAddr = line
        .trim()
        .strip_prefix("listening ")
        .unwrap_or_else(|| panic!("unexpected announcement {line:?}"))
        .parse()
        .unwrap();
    assert!(address.ip().is_loopback());
    assert_ne!(address.port(), 0);
    assert!(std::net::TcpStream::connect(address).is_ok());

    // The runtime ending closes this pipe, however it ends.
    drop(child.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("the managed vision proxy outlived its parent");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "{status:?}");
    assert!(std::net::TcpStream::connect(address).is_err());
}
