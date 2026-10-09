//! firefox endpoints diagnostics and probes.
use super::bootstrap_capabilities::engine_composition_finding;
use super::DoctorContext;
use crate::doctor::{
    compose_worker_factory, push_doctor_check, DoctorCheck, DoctorReport, DoctorStatus, Duration,
    Result, SocketAddr, Url,
};

use anyhow::Context;
use std::io::Read;
use std::io::Write;
pub(super) fn firefox_endpoints(
    context: &mut DoctorContext,
    report: &mut DoctorReport,
) -> Result<()> {
    let config = &context.config;
    let selection = &context.selection;
    // Enrolled BiDi endpoints that nothing is listening on. Held so the CDP-port
    // check below can join the two halves: a companion answering on the CDP port
    // while its enrolled port is dead is one fault, not two warnings.

    if let (Some(config), Some(selection)) = (&config, &selection) {
        match compose_worker_factory(config, selection.clone()) {
            Ok(_) => report.ok(
                "engine-satisfiability",
                "engine preference can be satisfied by configured registrations".to_string(),
            ),
            Err(error) => {
                let (status, name, detail) = engine_composition_finding(
                    &error,
                    crate::runtime_scopes::current_origin(),
                    selection.firefox.is_empty(),
                );
                report.record(status, name, detail);
            }
        }
        for profile in &selection.firefox {
            match Url::parse(&profile.bidi_url) {
                Ok(url) if matches!(url.scheme(), "ws" | "wss") => {
                    match probe_firefox_bidi(&profile.bidi_url) {
                        Ok(()) => report.ok(
                            "firefox-bidi",
                            format!("{} accepted a WebDriver BiDi handshake", profile.bidi_url),
                        ),
                        Err(BidiProbeFailure::Unreachable(error)) => {
                            context.unreachable_bidi.push(profile.bidi_url.clone());
                            report.warn(
                                "firefox-bidi",
                                format!(
                                    "nothing is listening on {} ({error:#}); the enrolled Firefox companion is not running there",
                                    profile.bidi_url,
                                ),
                            );
                        }
                        Err(failure @ BidiProbeFailure::NotBidi(_)) => {
                            report.warn(
                                "firefox-bidi",
                                format!(
                                    "{} answered but is not a Firefox WebDriver BiDi endpoint ({failure}); another service may own the port",
                                    profile.bidi_url,
                                ),
                            );
                        }
                    }
                }
                _ => {
                    report.fail(
                        "firefox-bidi",
                        format!(
                            "profile {} has an invalid bidiUrl (expected ws:// or wss://)",
                            profile.profile_id
                        ),
                    );
                }
            }
            if profile.profile_dir.exists() {
                report.ok(
                    "firefox-profile-dir",
                    profile.profile_dir.display().to_string(),
                );
            } else {
                report.warn(
                    "firefox-profile-dir",
                    format!("{} does not exist yet", profile.profile_dir.display()),
                );
            }
            match profile.companion_bind.parse::<SocketAddr>() {
                Ok(bind) => {
                    let companion_check = check_companion_port(bind);
                    push_doctor_check(report, companion_check);
                }
                Err(_) => {
                    report.fail(
                        "firefox-companion-bind",
                        format!(
                            "profile {} has an invalid companionBind",
                            profile.profile_id
                        ),
                    );
                }
            }
        }
    }

    Ok(())
}

pub(in crate::doctor) fn check_companion_port(bind: SocketAddr) -> DoctorCheck {
    let detail = if bind.port() == 0 {
        format!(
            "the runtime binds a free loopback port on {} at every start",
            bind.ip()
        )
    } else {
        format!(
            "the runtime binds a free loopback port on {} at every start; configured port {} is not used",
            bind.ip(),
            bind.port()
        )
    };
    DoctorCheck {
        status: DoctorStatus::Ok,
        name: "companion-port".to_string(),
        detail,
    }
}

pub(in crate::doctor) fn probe_firefox_bidi(
    endpoint: &str,
) -> std::result::Result<(), BidiProbeFailure> {
    use BidiProbeFailure::{NotBidi, Unreachable};
    let probe = || -> Result<std::net::TcpStream> {
        let url = Url::parse(endpoint).context("invalid WebDriver BiDi URL")?;
        if url.scheme() != "ws" {
            anyhow::bail!("doctor currently probes loopback ws:// BiDi endpoints only");
        }
        let port = url
            .port_or_known_default()
            .context("BiDi URL has no port")?;
        let address = url
            .socket_addrs(|| Some(port))?
            .into_iter()
            .next()
            .context("BiDi host resolved to no addresses")?;
        Ok(std::net::TcpStream::connect_timeout(
            &address,
            Duration::from_millis(500),
        )?)
    };
    let mut stream = probe().map_err(Unreachable)?;
    let mut handshake = || -> Result<String> {
        let url = Url::parse(endpoint)?;
        let host = url.host_str().context("BiDi URL has no host")?.to_owned();
        let port = url
            .port_or_known_default()
            .context("BiDi URL has no port")?;
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;
        stream.set_write_timeout(Some(Duration::from_secs(1)))?;
        let path = if url.path().is_empty() {
            "/".to_owned()
        } else {
            url.path().to_owned()
        };
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )?;
        let mut response = [0_u8; 4096];
        let read = stream.read(&mut response)?;
        let head = String::from_utf8_lossy(&response[..read]);
        Ok(head.lines().next().unwrap_or("empty response").to_owned())
    };
    let status = handshake().map_err(NotBidi)?;
    if !status.contains(" 101 ") {
        return Err(NotBidi(anyhow::anyhow!(
            "WebSocket handshake returned {status}"
        )));
    }
    Ok(())
}

pub(in crate::doctor) enum BidiProbeFailure {
    /// Nothing accepted the connection at that address.
    Unreachable(anyhow::Error),
    /// Something answered, but not with a WebSocket upgrade.
    NotBidi(anyhow::Error),
}

impl std::fmt::Display for BidiProbeFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(error) | Self::NotBidi(error) => write!(formatter, "{error:#}"),
        }
    }
}

#[cfg(test)]
mod bidi_probe_tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn firefox_bidi_probe_rejects_an_http_service_on_the_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut chunk = [0_u8; 256];
                let read = stream.read(&mut chunk).unwrap();
                assert_ne!(read, 0, "probe closed before completing the handshake");
                request.extend_from_slice(&chunk[..read]);
            }
            stream
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });

        let error = probe_firefox_bidi(&format!("ws://{address}/session")).unwrap_err();
        assert!(
            matches!(error, BidiProbeFailure::NotBidi(_)),
            "a live socket answering HTTP is not an unreachable endpoint: {error}"
        );
        assert!(error.to_string().contains("404"), "{error}");
        server.join().unwrap();
    }

    /// A refused connection is nobody listening, which is the opposite of the
    /// port being owned. Classifying both the same way sent operators hunting
    /// for a process that was never there.
    #[test]
    fn firefox_bidi_probe_separates_a_refused_connection_from_a_wrong_protocol() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let error = probe_firefox_bidi(&format!("ws://127.0.0.1:{port}/session")).unwrap_err();
        assert!(
            matches!(error, BidiProbeFailure::Unreachable(_)),
            "nothing is listening, so this is unreachable, not a protocol mismatch: {error}"
        );
    }
}
