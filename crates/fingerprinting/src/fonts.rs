//! Font enumeration masking.
//!
//! Returns a standardized, common font list instead of the user's
//! actual fonts to prevent fingerprinting via font enumeration.

use serde::{Deserialize, Serialize};

/// Configuration for font masking.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FontConfig {
    #[serde(default = "default_standard_fonts")]
    pub standard_fonts: Vec<String>,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            standard_fonts: default_standard_fonts(),
        }
    }
}

fn default_standard_fonts() -> Vec<String> {
    vec![
        "Arial".to_string(),
        "Arial Black".to_string(),
        "Calibri".to_string(),
        "Cambria".to_string(),
        "Comic Sans MS".to_string(),
        "Courier New".to_string(),
        "Georgia".to_string(),
        "Helvetica".to_string(),
        "Impact".to_string(),
        "Lucida Console".to_string(),
        "Lucida Sans Unicode".to_string(),
        "Microsoft Sans Serif".to_string(),
        "Palatino Linotype".to_string(),
        "Segoe UI".to_string(),
        "Tahoma".to_string(),
        "Times New Roman".to_string(),
        "Trebuchet MS".to_string(),
        "Verdana".to_string(),
    ]
}

/// Font masker that returns standardized font lists.
pub struct FontMasker {
    config: FontConfig,
}

impl FontMasker {
    pub fn new(config: FontConfig) -> Self {
        Self { config }
    }

    /// Get the standardized font list for enumeration masking.
    pub fn get_standard_fonts(&self) -> Vec<String> {
        if self.config.standard_fonts.is_empty() {
            default_standard_fonts()
        } else {
            self.config.standard_fonts.clone()
        }
    }
}
