//! Parse Firefox's `WebDriverBiDiServer.json` into a loopback BiDi session URL.

use std::{
    fs::OpenOptions,
    io::Read,
    path::Path,
};

use types::{CommandError, ErrorCode, ErrorLayer};
use url::Url;

const ENDPOINT_FILENAME: &str = "WebDriverBiDiServer.json";

/// Parse a `WebDriverBiDiServer.json` payload into `ws://{authority}/session`.
pub fn bidi_url_from_endpoint_file(bytes: &[u8]) -> Result<Url, String> {
    if bytes.len() > 4096 {
        return Err("Firefox BiDi endpoint file exceeds its bound".into());
    }
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|_| "Firefox BiDi endpoint file is malformed".to_owned())?;
    let object = value
        .as_object()
        .ok_or_else(|| "Firefox BiDi endpoint file must be an object".to_owned())?;
    if object.len() != 2 || !object.contains_key("ws_host") || !object.contains_key("ws_port") {
        return Err("Firefox BiDi endpoint file has an unsupported schema".into());
    }
    let host = object["ws_host"]
        .as_str()
        .ok_or_else(|| "Firefox BiDi endpoint host is invalid".to_owned())?;
    let address: std::net::IpAddr = host
        .parse()
        .map_err(|_| "Firefox BiDi endpoint host is invalid".to_owned())?;
    if !address.is_loopback() {
        return Err("Firefox BiDi endpoint must be loopback".into());
    }
    let port = object["ws_port"]
        .as_u64()
        .filter(|port| *port > 0 && *port <= u16::MAX as u64)
        .ok_or_else(|| "Firefox BiDi endpoint port is invalid".to_owned())?;
    let authority = match address {
        std::net::IpAddr::V4(address) => format!("{address}:{port}"),
        std::net::IpAddr::V6(address) => format!("[{address}]:{port}"),
    };
    Url::parse(&format!("ws://{authority}/session"))
        .map_err(|_| "Firefox BiDi endpoint URL is invalid".to_owned())
}

/// Read `$profile_dir/WebDriverBiDiServer.json` and parse it into a BiDi URL.
pub fn read_bidi_url_from_profile_dir(profile_dir: &Path) -> Result<Url, CommandError> {
    read_bidi_url_from_path(&profile_dir.join(ENDPOINT_FILENAME))
}

fn read_bidi_url_from_path(path: &Path) -> Result<Url, CommandError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path).map_err(endpoint_io_error)?;
    let metadata = file.metadata().map_err(endpoint_io_error)?;
    if !metadata.file_type().is_file() || metadata.len() > 4096 {
        return Err(endpoint_policy_error(
            "Firefox BiDi endpoint file is invalid",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    (&file)
        .take(4097)
        .read_to_end(&mut bytes)
        .map_err(endpoint_io_error)?;
    if bytes.len() > 4096 {
        return Err(endpoint_policy_error(
            "Firefox BiDi endpoint file exceeds its bound",
        ));
    }
    bidi_url_from_endpoint_file(&bytes).map_err(endpoint_launch_error)
}

fn endpoint_io_error(error: std::io::Error) -> CommandError {
    CommandError {
        code: ErrorCode::BrowserLaunchFailed,
        message: error.to_string(),
        layer: ErrorLayer::Driver,
        retryable: false,
    }
}

fn endpoint_policy_error(message: impl Into<String>) -> CommandError {
    CommandError {
        code: ErrorCode::PolicyDenied,
        message: message.into(),
        layer: ErrorLayer::Driver,
        retryable: false,
    }
}

fn endpoint_launch_error(message: String) -> CommandError {
    CommandError {
        code: ErrorCode::BrowserLaunchFailed,
        message,
        layer: ErrorLayer::Driver,
        retryable: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bidi_url_from_endpoint_file_accepts_loopback_port() {
        let bytes = br#"{"ws_host":"127.0.0.1","ws_port":9222}"#;
        let url = bidi_url_from_endpoint_file(bytes).expect("parse");
        assert_eq!(url.as_str(), "ws://127.0.0.1:9222/session");
    }

    #[test]
    fn bidi_url_from_endpoint_file_rejects_non_loopback() {
        let bytes = br#"{"ws_host":"8.8.8.8","ws_port":9222}"#;
        assert!(bidi_url_from_endpoint_file(bytes).is_err());
    }

    #[test]
    fn read_bidi_url_from_profile_dir_reads_endpoint_file() {
        let root = tempfile::tempdir().unwrap();
        let endpoint = root.path().join(ENDPOINT_FILENAME);
        std::fs::write(
            &endpoint,
            br#"{"ws_host":"127.0.0.1","ws_port":9222}"#,
        )
        .unwrap();
        let url = read_bidi_url_from_profile_dir(root.path()).expect("read");
        assert_eq!(url.as_str(), "ws://127.0.0.1:9222/session");
    }

    #[test]
    fn read_bidi_url_from_profile_dir_fails_when_missing() {
        let root = tempfile::tempdir().unwrap();
        assert!(read_bidi_url_from_profile_dir(root.path()).is_err());
    }
}
