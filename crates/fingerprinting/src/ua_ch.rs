//! User-Agent Client Hints profile generation.

use serde::{Deserialize, Serialize};

/// A single brand/version pair for Sec-CH-UA / userAgentData.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrandVersion {
    pub brand: String,
    pub version: String,
}

impl BrandVersion {
    pub fn new(brand: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            brand: brand.into(),
            version: version.into(),
        }
    }
}

/// Client Hints metadata aligned with a spoofed Chrome UA string.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClientHintsProfile {
    pub brands: Vec<BrandVersion>,
    pub full_version_list: Vec<BrandVersion>,
    /// UA-CH platform name (`Windows`, `macOS`, …) — not `navigator.platform`.
    pub platform: String,
    pub platform_version: String,
    pub architecture: String,
    pub bitness: String,
    pub model: String,
    pub mobile: bool,
    pub full_version: String,
}

/// Build Client Hints coherent with `chrome_major` and navigator `platform`.
pub fn build_client_hints(chrome_major: u32, navigator_platform: &str) -> ClientHintsProfile {
    let major = chrome_major.max(100);
    let full_version = format!("{major}.0.0.0");
    let (ch_platform, platform_version) = match navigator_platform {
        "MacIntel" => ("macOS", "14.0.0"),
        "Linux x86_64" => ("Linux", "6.5.0"),
        _ => ("Windows", "15.0.0"),
    };

    let brands = vec![
        BrandVersion::new("Not A(Brand", "8"),
        BrandVersion::new("Chromium", major.to_string()),
        BrandVersion::new("Google Chrome", major.to_string()),
    ];
    let full_version_list = vec![
        BrandVersion::new("Not A(Brand", "8.0.0.0"),
        BrandVersion::new("Chromium", &full_version),
        BrandVersion::new("Google Chrome", &full_version),
    ];

    ClientHintsProfile {
        brands,
        full_version_list,
        platform: ch_platform.to_string(),
        platform_version: platform_version.to_string(),
        architecture: "x86".to_string(),
        bitness: "64".to_string(),
        model: String::new(),
        mobile: false,
        full_version,
    }
}
