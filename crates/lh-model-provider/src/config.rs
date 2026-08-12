//! Provider configuration (architecture §13.2): layered TOML config,
//! multiple providers definable at once, API keys referenced by
//! environment variable name rather than stored in plaintext.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{ProviderError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderProtocol {
    Anthropic,
    OpenAiCompatible,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProviderConfig {
    pub name: String,
    pub protocol: ProviderProtocol,
    pub base_url: String,
    pub api_key_env: String,
    pub default_model: String,
    #[serde(default)]
    pub extra_headers: HashMap<String, String>,
    /// A manual override, not auto-discovered -- neither provider's public
    /// models-list response reliably reports context length in a normalized
    /// way. Applies to whichever model this config's `default_model` (or a
    /// later `model/select`) currently names. Omitted/absent when unset.
    #[serde(default)]
    pub context_window: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProvidersFile {
    /// Name of the provider used when no `--provider` flag is given.
    pub default: Option<String>,
    #[serde(rename = "provider", default)]
    pub providers: Vec<ModelProviderConfig>,
}

impl ProvidersFile {
    pub fn find(&self, name: &str) -> Option<&ModelProviderConfig> {
        self.providers.iter().find(|p| p.name == name)
    }

    pub fn default_provider(&self) -> Result<&ModelProviderConfig> {
        let name = self.default.as_deref().ok_or_else(|| {
            ProviderError::Config("no default provider configured".to_string())
        })?;
        self.find(name).ok_or_else(|| {
            ProviderError::Config(format!("default provider '{name}' not found in config"))
        })
    }
}

pub fn load_providers_file(path: &Path) -> Result<ProvidersFile> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| ProviderError::Config(format!("reading {}: {e}", path.display())))?;
    toml::from_str(&text).map_err(|e| ProviderError::Config(format!("parsing {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_multi_provider_toml_document() {
        let toml_text = r#"
            default = "anthropic"

            [[provider]]
            name = "anthropic"
            protocol = "anthropic"
            base_url = "https://api.anthropic.com"
            api_key_env = "ANTHROPIC_API_KEY"
            default_model = "claude-sonnet-5"

            [[provider]]
            name = "local-llama"
            protocol = "open-ai-compatible"
            base_url = "http://localhost:11434/v1"
            api_key_env = "OLLAMA_API_KEY"
            default_model = "llama3"
        "#;

        let file: ProvidersFile = toml::from_str(toml_text).unwrap();
        assert_eq!(file.default.as_deref(), Some("anthropic"));
        assert_eq!(file.providers.len(), 2);
        assert_eq!(file.find("local-llama").unwrap().protocol, ProviderProtocol::OpenAiCompatible);
        assert_eq!(file.default_provider().unwrap().name, "anthropic");
    }
}
