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
            match inspect_enrolled_firefox(profile) {
                Some(EnrolledFirefox::Live {
                    endpoint,
                    stale_companion,
                }) => {
                    report.ok(
                        "firefox-bidi",
                        format!("{endpoint} accepted a WebDriver BiDi handshake"),
                    );
                    if profile.attachment_only && stale_companion {
                        report.fail(
                            "firefox-companion-build",
                            "the companion was installed after this Firefox started; Firefox loads it only at start, and attachment-only mode refuses sessions until Firefox restarts".to_string(),
                        );
                    } else if profile.attachment_only {
                        report.ok(
                            "firefox-companion-build",
                            "Firefox started after the installed companion".to_string(),
                        );
                    }
                }
                Some(EnrolledFirefox::Down { endpoint, error }) => {
                    context.unreachable_bidi.push(endpoint.to_string());
                    if profile.attachment_only {
                        report.fail(
                            "firefox-bidi",
                            format!(
                                "nothing is listening on {endpoint} ({error:#}); attachment-only mode refuses sessions until the enrolled Firefox runs"
                            ),
                        );
                    } else {
                        report.warn(
                            "firefox-bidi",
                            format!(
                                "nothing is listening on {endpoint} ({error:#}); the enrolled Firefox companion is not running there"
                            ),
                        );
                    }
                }
                Some(EnrolledFirefox::NotBidi { endpoint, failure }) => {
                    report.warn(
                        "firefox-bidi",
                        format!(
                            "{endpoint} answered but is not a Firefox WebDriver BiDi endpoint ({failure}); another service may own the port"
                        ),
                    );
                }
                None => {
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

/// What the enrolled profile's Firefox looks like from its live BiDi endpoint.
pub(in crate::doctor) enum EnrolledFirefox {
    Live {
        endpoint: Url,
        stale_companion: bool,
    },
    Down {
        endpoint: Url,
        error: anyhow::Error,
    },
    NotBidi {
        endpoint: Url,
        failure: BidiProbeFailure,
    },
}

impl EnrolledFirefox {
    /// Attachment-only mode refuses sessions in these states and never restarts Firefox itself.
    pub(in crate::doctor) fn needs_restart(&self) -> bool {
        matches!(
            self,
            Self::Live {
                stale_companion: true,
                ..
            } | Self::Down { .. }
        )
    }
}

/// Probe the endpoint the runtime connects to. `None` when the enrolled
/// `bidiUrl` is not a ws:// or wss:// URL.
pub(in crate::doctor) fn inspect_enrolled_firefox(
    profile: &config::FirefoxCompanionConfig,
) -> Option<EnrolledFirefox> {
    let enrolled = Url::parse(&profile.bidi_url)
        .ok()
        .filter(|url| matches!(url.scheme(), "ws" | "wss"))?;
    let endpoint = firefox_companion::selection::live_bidi_url(&profile.profile_dir, &enrolled);
    Some(match probe_firefox_bidi(endpoint.as_str()) {
        Ok(()) => EnrolledFirefox::Live {
            stale_companion: firefox_companion::selection::companion_installed_after_start(
                &profile.profile_dir,
            ),
            endpoint,
        },
        Err(BidiProbeFailure::Unreachable(error)) => EnrolledFirefox::Down { endpoint, error },
        Err(failure @ BidiProbeFailure::NotBidi(_)) => {
            EnrolledFirefox::NotBidi { endpoint, failure }
        }
    })
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

#[cfg(test)]
mod enrolled_firefox_tests {
    use super::super::test_support;
    use super::*;
    use crate::doctor::DoctorColorMode;
    use config::{AppConfig, BrowserSelectionConfig, FirefoxCompanionConfig};
    use std::io::{Read, Write};
    use std::path::Path;
    use std::time::SystemTime;

    /// Answers every connection with a WebSocket upgrade, as Firefox's BiDi listener does.
    fn bidi_listener() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut chunk = [0_u8; 256];
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => request.extend_from_slice(&chunk[..read]),
                    }
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
                );
            }
        });
        port
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn enrolled(root: &Path, port: u16, attachment_only: bool) -> FirefoxCompanionConfig {
        let profile_dir = root.join("firefox-profile");
        std::fs::create_dir_all(profile_dir.join("extensions")).unwrap();
        FirefoxCompanionConfig {
            profile_id: uuid::Uuid::new_v4().to_string(),
            bidi_url: format!("ws://127.0.0.1:{port}/session"),
            profile_dir,
            companion_bind: "127.0.0.1:0".to_string(),
            descriptor_path: root.join("firefox-native-host-descriptor.json"),
            attachment_only,
            timeout_ms: 30_000,
            pairing_code_ttl_ms: 300_000,
            attachment_ttl_ms: 300_000,
        }
    }

    /// The endpoint file Firefox writes when its BiDi listener starts.
    fn publish_endpoint(profile: &FirefoxCompanionConfig, port: u16, at: SystemTime) {
        let path = profile.profile_dir.join("WebDriverBiDiServer.json");
        std::fs::write(
            &path,
            format!("{{\"ws_host\":\"127.0.0.1\",\"ws_port\":{port}}}"),
        )
        .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(at)
            .unwrap();
    }

    fn install_companion(profile: &FirefoxCompanionConfig) {
        let dir = profile
            .profile_dir
            .join("extensions/firefox-companion@bobby-browser.local");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("build-id.json"),
            br#"{"buildId":"22563e26dc3ef3469eeeba24df3c104f"}"#,
        )
        .unwrap();
    }

    fn doctor(root: &Path, profile: &FirefoxCompanionConfig) -> (DoctorReport, String) {
        let mut context = test_support::context(root);
        context.config = Some(AppConfig::default());
        context.selection = Some(BrowserSelectionConfig {
            preference: Default::default(),
            firefox: vec![profile.clone()],
            chromium: Vec::new(),
        });
        let mut report = DoctorReport::default();
        firefox_endpoints(&mut context, &mut report).unwrap();
        let mut rendered = Vec::new();
        report
            .render_to(&mut rendered, DoctorColorMode::Never)
            .unwrap();
        (report, String::from_utf8(rendered).unwrap())
    }

    fn line<'a>(rendered: &'a str, check: &str) -> &'a str {
        rendered
            .lines()
            .find(|line| line.contains(&format!("] {check}:")))
            .unwrap_or_else(|| panic!("no {check} line in:\n{rendered}"))
    }

    #[test]
    fn firefox_bidi_probes_the_port_firefox_published_in_the_profile() {
        let root = tempfile::tempdir().unwrap();
        let live = bidi_listener();
        let profile = enrolled(root.path(), free_port(), false);
        publish_endpoint(&profile, live, SystemTime::now());

        let (_, rendered) = doctor(root.path(), &profile);
        let bidi = line(&rendered, "firefox-bidi");
        assert!(bidi.starts_with("[ok]"), "{rendered}");
        assert!(bidi.contains(&format!("127.0.0.1:{live}")), "{rendered}");
    }

    #[test]
    fn attachment_only_firefox_started_before_the_companion_install_fails_until_restarted() {
        let root = tempfile::tempdir().unwrap();
        let live = bidi_listener();
        let profile = enrolled(root.path(), live, true);
        publish_endpoint(
            &profile,
            live,
            SystemTime::now() - std::time::Duration::from_secs(3600),
        );
        install_companion(&profile);

        let (_, rendered) = doctor(root.path(), &profile);
        let build = line(&rendered, "firefox-companion-build");
        assert!(build.starts_with("[fail]"), "{rendered}");
        assert!(build.contains("fix: bobby doctor --fix"), "{rendered}");

        publish_endpoint(
            &profile,
            live,
            SystemTime::now() + std::time::Duration::from_secs(60),
        );
        let (report, rendered) = doctor(root.path(), &profile);
        assert!(
            report
                .checks
                .iter()
                .filter(|check| check.name.starts_with("firefox"))
                .all(|check| check.status == DoctorStatus::Ok),
            "{rendered}"
        );
    }

    #[test]
    fn attachment_only_firefox_that_is_not_running_fails_with_the_fix() {
        let root = tempfile::tempdir().unwrap();
        let profile = enrolled(root.path(), free_port(), true);

        let (_, rendered) = doctor(root.path(), &profile);
        let bidi = line(&rendered, "firefox-bidi");
        assert!(bidi.starts_with("[fail]"), "{rendered}");
        assert!(bidi.contains("fix: bobby doctor --fix"), "{rendered}");
    }
}
