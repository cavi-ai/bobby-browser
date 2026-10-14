use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    io::Read,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use config::VisionProviderConfig;
use dialoguer::{theme::ColorfulTheme, FuzzySelect, Input};
use serde_json::Value;

const MAX_BODY: usize = 1024 * 1024;
const MAX_MODELS: usize = 512;
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Model {
    pub(crate) id: String,
    pub(crate) vision: Option<bool>,
}

fn vision_capability(model: &Value) -> Option<bool> {
    if model
        .get("output_modalities")
        .or_else(|| model.pointer("/architecture/output_modalities"))
        .and_then(Value::as_array)
        .is_some_and(|modalities| {
            !modalities
                .iter()
                .any(|value| value.as_str() == Some("text"))
        })
    {
        return Some(false);
    }
    if let Some(vision) = model
        .pointer("/capabilities/vision")
        .and_then(Value::as_bool)
    {
        return Some(vision);
    }
    if let Some(capabilities) = model.get("capabilities").and_then(Value::as_array) {
        return Some(
            capabilities
                .iter()
                .any(|value| value.as_str() == Some("vision")),
        );
    }
    model
        .get("input_modalities")
        .or_else(|| model.pointer("/architecture/input_modalities"))
        .and_then(Value::as_array)
        .map(|modalities| {
            modalities
                .iter()
                .any(|value| value.as_str() == Some("image"))
        })
}

fn valid_model_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 512 && id.trim() == id && !id.chars().any(char::is_control)
}

fn parse_models(body: &Value) -> Result<Vec<Model>> {
    let entries = body
        .get("models")
        .or_else(|| body.get("data"))
        .and_then(Value::as_array)
        .context("provider returned an unrecognized model list")?;
    if entries.len() > MAX_MODELS {
        bail!("provider model list exceeds {MAX_MODELS} entries");
    }
    let mut models = Vec::new();
    for entry in entries {
        if matches!(
            entry.get("type").and_then(Value::as_str),
            Some("embedding" | "decision")
        ) {
            continue;
        }
        let Some(id) = entry
            .get("key")
            .or_else(|| entry.get("id"))
            .or_else(|| entry.get("name"))
            .and_then(Value::as_str)
            .filter(|id| valid_model_id(id))
        else {
            continue;
        };
        let vision = vision_capability(entry);
        if vision != Some(false) {
            models.push(Model {
                id: id.to_owned(),
                vision,
            });
        }
    }
    sort_models(&mut models);
    Ok(models)
}

fn sort_models(models: &mut Vec<Model>) {
    models.sort_by(|left, right| {
        (left.vision != Some(true), &left.id).cmp(&(right.vision != Some(true), &right.id))
    });
    let mut seen = std::collections::HashSet::new();
    models.retain(|model| seen.insert(model.id.clone()));
}

fn read_json(mut source: impl Read) -> Result<Value> {
    let mut bytes = Vec::new();
    source
        .by_ref()
        .take((MAX_BODY + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow!("could not read provider model metadata"))?;
    if bytes.len() > MAX_BODY {
        bail!("provider model metadata exceeds 1 MiB");
    }
    serde_json::from_slice(&bytes).map_err(|_| anyhow!("provider returned invalid model metadata"))
}

fn request_json(
    client: &reqwest::blocking::Client,
    url: &str,
    body: Option<Value>,
    token: Option<&str>,
    deadline: Instant,
) -> Result<Option<Value>> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .context("model discovery timed out")?;
    let mut request = match body {
        Some(body) => client.post(url).json(&body),
        None => client.get(url),
    }
    .timeout(remaining);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .map_err(|_| anyhow!("could not reach the provider model API"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        bail!(
            "provider model API returned HTTP {}",
            response.status().as_u16()
        );
    }
    read_json(response).map(Some)
}

fn query_http(
    provider: &str,
    profile: &VisionProviderConfig,
    token: Option<&str>,
) -> Result<Vec<Model>> {
    let url = url::Url::parse(&profile.base_url).context("invalid provider base URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("use an HTTP(S) provider URL without credentials or query parameters");
    }
    let mut client =
        reqwest::blocking::Client::builder().redirect(reqwest::redirect::Policy::none());
    if match url.host() {
        Some(url::Host::Domain(host)) => host == "localhost",
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    } {
        client = client.no_proxy();
    }
    let client = client
        .build()
        .context("could not create model discovery client")?;
    let deadline = Instant::now() + QUERY_TIMEOUT;
    let base = profile.base_url.trim_end_matches('/');
    let root = base
        .strip_suffix("/v1/chat/completions")
        .or_else(|| base.strip_suffix("/chat/completions"))
        .or_else(|| base.strip_suffix("/v1"))
        .unwrap_or(base);
    if provider == "ollama" {
        if let Some(body) =
            request_json(&client, &format!("{root}/api/tags"), None, token, deadline)?
        {
            let mut models = parse_models(&body)?;
            for model in &mut models {
                if model.vision.is_none() {
                    let metadata = request_json(
                        &client,
                        &format!("{root}/api/show"),
                        Some(serde_json::json!({"model":model.id})),
                        token,
                        deadline,
                    )
                    .ok()
                    .flatten();
                    model.vision = metadata.as_ref().and_then(vision_capability);
                }
            }
            models.retain(|model| model.vision != Some(false));
            sort_models(&mut models);
            return Ok(models);
        }
    } else if provider == "lmstudio" {
        if let Some(body) = request_json(
            &client,
            &format!("{root}/api/v1/models"),
            None,
            token,
            deadline,
        )? {
            return parse_models(&body);
        }
    }
    let models_url = if matches!(provider, "ollama" | "lmstudio") {
        format!("{root}/v1/models")
    } else {
        format!("{base}/models")
    };
    let body = request_json(&client, &models_url, None, token, deadline)?
        .context("provider has no model discovery endpoint")?;
    parse_models(&body)
}

pub(crate) fn cached_mlx_models() -> Result<Vec<Model>> {
    let root = crate::vision_readiness::hugging_face_cache_root()?;
    let Ok(entries) = std::fs::read_dir(root) else {
        return Ok(Vec::new());
    };
    let mut models = Vec::new();
    for entry in entries.flatten().take(MAX_MODELS) {
        let folder = entry.file_name().to_string_lossy().into_owned();
        let Some((owner, name)) = folder
            .strip_prefix("models--")
            .and_then(|name| name.split_once("--"))
        else {
            continue;
        };
        let id = format!("{owner}/{name}");
        if !valid_model_id(&id) {
            continue;
        }
        let Ok(snapshots) = std::fs::read_dir(entry.path().join("snapshots")) else {
            continue;
        };
        for snapshot in snapshots.flatten().take(32) {
            let path = snapshot.path();
            if !path.join("preprocessor_config.json").is_file() {
                continue;
            }
            let Ok(file) = std::fs::File::open(path.join("config.json")) else {
                continue;
            };
            if read_json(file)
                .ok()
                .is_some_and(|value| value.get("vision_config").is_some_and(Value::is_object))
            {
                models.push(Model {
                    id,
                    vision: Some(true),
                });
                break;
            }
        }
    }
    sort_models(&mut models);
    Ok(models)
}

#[derive(PartialEq, Eq)]
struct CacheKey {
    provider: String,
    endpoint: String,
    credential: u64,
    hf_home: Option<std::path::PathBuf>,
}

struct CacheEntry {
    key: CacheKey,
    fetched: Instant,
    models: Vec<Model>,
}

static CACHE: OnceLock<Mutex<Vec<CacheEntry>>> = OnceLock::new();

pub(crate) fn discover(
    provider: &str,
    profile: &VisionProviderConfig,
    refresh: bool,
) -> Result<Vec<Model>> {
    let token = if provider == "mlx" {
        None
    } else {
        profile
            .api_key_env
            .as_deref()
            .map(|name| {
                std::env::var(name)
                    .ok()
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| anyhow!("set {name} to query this provider's available models"))
            })
            .transpose()?
    };
    let mut fingerprint = DefaultHasher::new();
    token.hash(&mut fingerprint);
    let key = CacheKey {
        provider: provider.to_owned(),
        endpoint: profile.base_url.clone(),
        credential: fingerprint.finish(),
        hf_home: (provider == "mlx")
            .then(crate::vision_readiness::hugging_face_cache_root)
            .transpose()?,
    };
    let cache = CACHE.get_or_init(|| Mutex::new(Vec::new()));
    if !refresh {
        if let Ok(entries) = cache.lock() {
            if let Some(entry) = entries
                .iter()
                .find(|entry| entry.key == key && entry.fetched.elapsed() < CACHE_TTL)
            {
                return Ok(entry.models.clone());
            }
        }
    }
    let provider_name = provider.to_owned();
    let profile = profile.clone();
    let models = std::thread::spawn(move || {
        if provider_name == "mlx" {
            cached_mlx_models()
        } else {
            query_http(&provider_name, &profile, token.as_deref())
        }
    })
    .join()
    .map_err(|_| anyhow!("model discovery failed"))??;
    if let Ok(mut entries) = cache.lock() {
        entries.retain(|entry| entry.key != key && entry.fetched.elapsed() < CACHE_TTL);
        if entries.len() >= 16 {
            entries.remove(0);
        }
        entries.push(CacheEntry {
            key,
            fetched: Instant::now(),
            models: models.clone(),
        });
    }
    Ok(models)
}

pub(crate) fn pick(provider: &str, profile: &VisionProviderConfig) -> Result<Option<String>> {
    let theme = ColorfulTheme::default();
    let mut refresh = false;
    loop {
        let catalog = discover(provider, profile, refresh);
        let models = match &catalog {
            Ok(models) => models.as_slice(),
            Err(error) => {
                eprintln!(
                    "Model list unavailable: {error}. Check the server or API key, then Refresh."
                );
                &[]
            }
        };
        let mut labels: Vec<String> = models
            .iter()
            .map(|model| {
                if model.vision == Some(true) {
                    format!("{} (vision)", model.id)
                } else {
                    format!("{} (image support not reported)", model.id)
                }
            })
            .collect();
        labels.extend([
            "Enter a model ID manually".into(),
            "Refresh available models".into(),
            "Back".into(),
        ]);
        let default = models
            .iter()
            .position(|model| model.id == profile.model)
            .unwrap_or(0);
        let Some(choice) = FuzzySelect::with_theme(&theme)
            .with_prompt(format!("{provider} model — type to search"))
            .items(&labels)
            .default(default)
            .interact_opt()?
        else {
            return Ok(None);
        };
        if choice < models.len() {
            let selected = &models[choice].id;
            let current = discover(provider, profile, true)?;
            if !current.iter().any(|model| &model.id == selected) {
                eprintln!("That model is no longer available. Choose another model.");
                refresh = true;
                continue;
            }
            return Ok(Some(selected.clone()));
        }
        match choice - models.len() {
            0 => {
                let id = Input::<String>::with_theme(&theme)
                    .with_prompt("Exact model ID")
                    .validate_with(|id: &String| {
                        if valid_model_id(id) {
                            Ok(())
                        } else {
                            Err("enter a non-empty model ID without control characters")
                        }
                    })
                    .interact_text()?;
                if let Ok(models) = &catalog {
                    if !models.iter().any(|model| model.id == id) {
                        eprintln!("That ID is not in the provider's available model list. Choose a listed model or Refresh.");
                        continue;
                    }
                    if !discover(provider, profile, true)?
                        .iter()
                        .any(|model| model.id == id)
                    {
                        eprintln!(
                            "That model is no longer available. Refresh and choose another model."
                        );
                        refresh = true;
                        continue;
                    }
                }
                return Ok(Some(id));
            }
            1 => refresh = true,
            _ => return Ok(None),
        }
    }
}

pub(crate) fn default_ollama_model(profile: &VisionProviderConfig) -> Result<String> {
    let Ok(models) = discover("ollama", profile, false) else {
        return Ok(profile.model.clone());
    };
    if let Some(model) = models.iter().find(|model| model.id == profile.model) {
        return Ok(model.id.clone());
    }
    if models.len() == 1 && models[0].vision == Some(true) {
        return Ok(models[0].id.clone());
    }
    bail!("choose an available Ollama vision model with --model (vision connect) or --vision-model (install); run interactively to select from the model list")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn serve(responses: Vec<(&'static str, u16, Value)>) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let thread = std::thread::spawn(move || {
            for (expected, status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0_u8; 4096];
                    let read = stream.read(&mut chunk).unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    if let Some(header_end) =
                        request.windows(4).position(|part| part == b"\r\n\r\n")
                    {
                        let headers =
                            String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if request.len() >= header_end + 4 + length {
                            break;
                        }
                    }
                }
                let request = String::from_utf8_lossy(&request);
                assert!(
                    request.contains(expected),
                    "unexpected discovery request: {request}"
                );
                let body = body.to_string();
                write!(stream, "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        (format!("http://{address}"), thread)
    }

    fn profile(base_url: String) -> VisionProviderConfig {
        VisionProviderConfig {
            base_url,
            model: "operator-model:tag".into(),
            api_key_env: None,
        }
    }

    struct EnvGuard {
        name: &'static str,
        previous: Option<std::ffi::OsString>,
    }
    impl EnvGuard {
        fn set(name: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let guard = Self {
                name,
                previous: std::env::var_os(name),
            };
            unsafe {
                std::env::set_var(name, value);
            }
            guard
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe {
                    std::env::set_var(self.name, value);
                },
                None => unsafe {
                    std::env::remove_var(self.name);
                },
            }
        }
    }

    #[test]
    fn ollama_discovery_queries_capabilities_when_tags_do_not_advertise_them() {
        let _lock = crate::onboarding::INSTALL_ENV_LOCK.lock().unwrap();
        let (base, server) = serve(vec![
            (
                "GET /api/tags",
                200,
                json!({"models":[{"name":"text:base"},{"name":"vision:custom"}]}),
            ),
            (
                "\"model\":\"text:base\"",
                200,
                json!({"capabilities":["completion"]}),
            ),
            (
                "\"model\":\"vision:custom\"",
                200,
                json!({"capabilities":["completion","vision"]}),
            ),
        ]);
        let models = discover("ollama", &profile(format!("{base}/v1")), true).unwrap();
        assert_eq!(
            models,
            vec![Model {
                id: "vision:custom".into(),
                vision: Some(true)
            }]
        );
        server.join().unwrap();
    }

    #[test]
    fn one_unavailable_ollama_model_does_not_hide_other_image_models() {
        let _lock = crate::onboarding::INSTALL_ENV_LOCK.lock().unwrap();
        let (base, server) = serve(vec![
            (
                "GET /api/tags",
                200,
                json!({"models":[
                    {"name":"image-model:ready","capabilities":["vision"]},
                    {"name":"unavailable:custom"}
                ]}),
            ),
            (
                "POST /api/show",
                500,
                json!({"error":"metadata unavailable"}),
            ),
        ]);
        let models = discover("ollama", &profile(base), true).unwrap();
        assert_eq!(
            models[0],
            Model {
                id: "image-model:ready".into(),
                vision: Some(true)
            }
        );
        assert_eq!(
            models[1],
            Model {
                id: "unavailable:custom".into(),
                vision: None
            }
        );
        server.join().unwrap();
    }

    #[test]
    fn lmstudio_discovery_uses_native_capability_metadata() {
        let _lock = crate::onboarding::INSTALL_ENV_LOCK.lock().unwrap();
        let (base, server) = serve(vec![(
            "GET /api/v1/models",
            200,
            json!({"models":[
                {"key":"local/image-model","type":"llm","capabilities":{"vision":true}},
                {"key":"local/text","type":"llm","capabilities":{"vision":false}}
            ]}),
        )]);
        let models = discover("lmstudio", &profile(format!("{base}/v1")), true).unwrap();
        assert_eq!(
            models,
            vec![Model {
                id: "local/image-model".into(),
                vision: Some(true)
            }]
        );
        server.join().unwrap();
    }

    #[test]
    fn discovery_caches_until_refresh_and_does_not_share_another_endpoint() {
        let _lock = crate::onboarding::INSTALL_ENV_LOCK.lock().unwrap();
        let (base, server) = serve(vec![
            (
                "GET /v1/models",
                200,
                json!({"data":[{"id":"first-model"}]}),
            ),
            (
                "GET /v1/models",
                200,
                json!({"data":[{"id":"replacement-model"}]}),
            ),
        ]);
        let configured = profile(format!("{base}/v1"));
        assert_eq!(
            discover("custom", &configured, false).unwrap()[0].id,
            "first-model"
        );
        assert_eq!(
            discover("custom", &configured, false).unwrap()[0].id,
            "first-model"
        );
        assert_eq!(
            discover("custom", &configured, true).unwrap()[0].id,
            "replacement-model"
        );
        server.join().unwrap();
        let (base, server) = serve(vec![(
            "GET /v1/models",
            200,
            json!({"data":[{"id":"other-account-model"}]}),
        )]);
        assert_eq!(
            discover("custom", &profile(format!("{base}/v1")), false).unwrap()[0].id,
            "other-account-model"
        );
        server.join().unwrap();
    }

    #[test]
    fn discovery_does_not_reuse_models_after_credentials_change() {
        let _lock = crate::onboarding::INSTALL_ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::set("BOBBY_MODEL_CATALOG_TEST_TOKEN", "first-fixture-token");
        let (base, server) = serve(vec![
            (
                "Bearer first-fixture-token",
                200,
                json!({"data":[{"id":"first-account"}]}),
            ),
            (
                "Bearer second-fixture-token",
                200,
                json!({"data":[{"id":"second-account"}]}),
            ),
        ]);
        let mut configured = profile(format!("{base}/v1"));
        configured.api_key_env = Some("BOBBY_MODEL_CATALOG_TEST_TOKEN".into());
        assert_eq!(
            discover("openai", &configured, false).unwrap()[0].id,
            "first-account"
        );
        unsafe {
            std::env::set_var("BOBBY_MODEL_CATALOG_TEST_TOKEN", "second-fixture-token");
        }
        assert_eq!(
            discover("openai", &configured, false).unwrap()[0].id,
            "second-account"
        );
        server.join().unwrap();
    }

    #[test]
    fn discovery_errors_do_not_expose_provider_response_bodies() {
        let _lock = crate::onboarding::INSTALL_ENV_LOCK.lock().unwrap();
        let (base, server) = serve(vec![(
            "GET /v1/models",
            401,
            json!({"error":"PRIVATE_PROVIDER_BODY"}),
        )]);
        let error = discover("openai", &profile(format!("{base}/v1")), true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("401"));
        assert!(!error.contains("PRIVATE_PROVIDER_BODY"));
        server.join().unwrap();
    }

    #[test]
    fn discovery_rejects_oversized_metadata_and_control_characters() {
        assert!(read_json(std::io::Cursor::new(vec![b' '; MAX_BODY + 1])).is_err());
        let models =
            parse_models(&json!({"data":[{"id":"bad\u{1b}[0m"},{"id":"good-model"}]})).unwrap();
        assert_eq!(
            models,
            vec![Model {
                id: "good-model".into(),
                vision: None
            }]
        );
    }

    #[test]
    fn mlx_discovery_lists_cached_image_models_and_ignores_text_only_snapshots() {
        let _lock = crate::onboarding::INSTALL_ENV_LOCK.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set("HF_HOME", root.path());
        for (name, metadata) in [
            ("models--org--image-model", json!({"vision_config":{}})),
            ("models--org--text-model", json!({"model_type":"text"})),
        ] {
            let snapshot = root
                .path()
                .join("hub")
                .join(name)
                .join("snapshots/revision");
            std::fs::create_dir_all(&snapshot).unwrap();
            std::fs::write(snapshot.join("config.json"), metadata.to_string()).unwrap();
            std::fs::write(snapshot.join("preprocessor_config.json"), "{}").unwrap();
        }
        let models = discover("mlx", &profile("http://127.0.0.1:9".into()), true).unwrap();
        assert_eq!(
            models,
            vec![Model {
                id: "org/image-model".into(),
                vision: Some(true)
            }]
        );
    }

    #[test]
    fn ollama_catalog_keeps_exact_vision_tags_and_excludes_text_models() {
        let models = parse_models(&json!({"models":[
            {"name":"my-vision:Q4_K_M","capabilities":["completion","vision"]},
            {"name":"text-only:8b","capabilities":["completion"]},
            {"name":"llava-looking-name:custom"}
        ]}))
        .unwrap();
        assert_eq!(
            models,
            vec![
                Model {
                    id: "my-vision:Q4_K_M".into(),
                    vision: Some(true)
                },
                Model {
                    id: "llava-looking-name:custom".into(),
                    vision: None
                },
            ]
        );
    }

    #[test]
    fn lmstudio_catalog_uses_model_keys_and_vision_metadata() {
        let models = parse_models(&json!({"models":[
            {"key":"local/model@q4","type":"llm","capabilities":{"vision":true}},
            {"key":"local/text","type":"llm","capabilities":{"vision":false}},
            {"key":"local/embedding","type":"embedding"}
        ]}))
        .unwrap();
        assert_eq!(
            models,
            vec![Model {
                id: "local/model@q4".into(),
                vision: Some(true)
            }]
        );
    }

    #[test]
    fn compatible_catalog_does_not_infer_vision_from_a_model_name() {
        let models = parse_models(&json!({"data":[{"id":"vision-sounding-name"}]})).unwrap();
        assert_eq!(
            models,
            vec![Model {
                id: "vision-sounding-name".into(),
                vision: None
            }]
        );
    }

    #[test]
    fn compatible_catalog_uses_input_and_output_modalities() {
        let models = parse_models(&json!({"data":[
            {"id":"image-input","architecture":{"input_modalities":["text","image"],"output_modalities":["text"]}},
            {"id":"text-input","architecture":{"input_modalities":["text"],"output_modalities":["text"]}},
            {"id":"image-output","architecture":{"input_modalities":["image"],"output_modalities":["image"]}}
        ]})).unwrap();
        assert_eq!(
            models,
            vec![Model {
                id: "image-input".into(),
                vision: Some(true)
            }]
        );
    }

    #[test]
    fn model_catalog_rejects_unrecognized_responses() {
        assert!(parse_models(&json!({"unexpected":[]})).is_err());
    }

    #[test]
    fn compatible_discovery_preserves_the_configured_api_prefix() {
        let _lock = crate::onboarding::INSTALL_ENV_LOCK.lock().unwrap();
        let (base, server) = serve(vec![(
            "GET /tenant/api/models",
            200,
            json!({"data":[{"id":"tenant-model"}]}),
        )]);
        let models = discover("custom", &profile(format!("{base}/tenant/api")), true).unwrap();
        assert_eq!(models[0].id, "tenant-model");
        server.join().unwrap();
    }

    #[test]
    fn automatic_ollama_setup_uses_the_only_available_image_model_exactly() {
        let _lock = crate::onboarding::INSTALL_ENV_LOCK.lock().unwrap();
        let (base, server) = serve(vec![(
            "GET /api/tags",
            200,
            json!({"models":[
                {"name":"image-model:custom","capabilities":["completion","vision"]},
                {"name":"text-model:custom","capabilities":["completion"]}
            ]}),
        )]);
        let mut configured = profile(base);
        configured.model = "llava:7b".into();
        assert_eq!(
            default_ollama_model(&configured).unwrap(),
            "image-model:custom"
        );
        server.join().unwrap();
    }

    #[test]
    fn automatic_ollama_setup_requires_selection_when_no_default_matches_multiple_models() {
        let _lock = crate::onboarding::INSTALL_ENV_LOCK.lock().unwrap();
        let (base, server) = serve(vec![(
            "GET /api/tags",
            200,
            json!({"models":[
                {"name":"image-model:a","capabilities":["vision"]},
                {"name":"image-model:b","capabilities":["vision"]}
            ]}),
        )]);
        let configured = profile(base);
        assert!(default_ollama_model(&configured).is_err());
        server.join().unwrap();
    }
}
