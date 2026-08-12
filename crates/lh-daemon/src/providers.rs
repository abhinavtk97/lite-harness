//! Resolves which model provider `lite-harnessd` should use for the native
//! agent loop (architecture §13.2), from a TOML config file. Overridable
//! via env vars so this is testable without touching `$HOME` or needing a
//! real API key -- `LITE_HARNESS_PROVIDERS_FILE` points at the file,
//! `LITE_HARNESS_PROVIDER` picks a provider by name instead of the file's
//! `default`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use lh_model_provider::{ModelProvider, ModelProviderConfig};

/// `Clone` (a cheap `Arc` bump plus a couple of small owned fields) so a
/// connection can seed its own mutable copy from the daemon-wide default at
/// connection start (architecture Phase 10.2) -- switching models on one
/// connection must never mutate what other connections see.
#[derive(Clone)]
pub struct ResolvedProvider {
    pub provider: Arc<dyn ModelProvider>,
    pub name: String,
    pub model: String,
    pub context_window: Option<u64>,
    /// The config this provider was built from -- kept around (not just
    /// discarded after `build_provider`) so `model/select` can rebuild a
    /// provider around a different model id without needing a whole second
    /// config-file read.
    pub config: ModelProviderConfig,
}

fn providers_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("LITE_HARNESS_PROVIDERS_FILE") {
        return Some(PathBuf::from(p));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/lite-harness/providers.toml"))
}

pub fn resolve_default_provider() -> Result<Option<ResolvedProvider>> {
    let Some(path) = providers_path() else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }

    let file = lh_model_provider::load_providers_file(&path)
        .with_context(|| format!("loading provider config from {}", path.display()))?;

    let cfg = match std::env::var("LITE_HARNESS_PROVIDER").ok() {
        Some(name) => file
            .find(&name)
            .with_context(|| format!("provider '{name}' not found in {}", path.display()))?,
        None => file
            .default_provider()
            .with_context(|| format!("resolving default provider from {}", path.display()))?,
    };

    let provider = lh_model_provider::build_provider(cfg)
        .with_context(|| format!("building provider '{}'", cfg.name))?;

    Ok(Some(ResolvedProvider {
        provider,
        name: cfg.name.clone(),
        model: cfg.default_model.clone(),
        context_window: cfg.context_window,
        config: cfg.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `providers_path`/`resolve_default_provider` read process-global env
    /// vars (`LITE_HARNESS_PROVIDERS_FILE`, `LITE_HARNESS_PROVIDER`,
    /// `HOME`) -- `cargo test` runs a binary crate's unit tests on multiple
    /// threads by default, so every test here must serialize through this
    /// lock and explicitly set every relevant var (never rely on another
    /// test having left one unset).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn write_providers_toml(dir: &std::path::Path, contents: &str) -> std::path::PathBuf {
        let path = dir.join("providers.toml");
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn no_env_override_and_no_home_yields_none() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("LITE_HARNESS_PROVIDERS_FILE");
        std::env::remove_var("LITE_HARNESS_PROVIDER");
        std::env::remove_var("HOME");

        assert!(providers_path().is_none());
        assert!(resolve_default_provider().unwrap().is_none());
    }

    #[test]
    fn providers_file_env_var_pointing_at_a_nonexistent_path_yields_none() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("LITE_HARNESS_PROVIDERS_FILE", "/definitely/does/not/exist/providers.toml");
        std::env::remove_var("LITE_HARNESS_PROVIDER");
        std::env::remove_var("HOME");

        assert!(resolve_default_provider().unwrap().is_none());
    }

    #[test]
    fn resolves_the_default_provider_from_the_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = write_providers_toml(
            dir.path(),
            "default = \"mock\"\n\n[[provider]]\nname = \"mock\"\nprotocol = \"open-ai-compatible\"\n\
             base_url = \"http://127.0.0.1:1\"\napi_key_env = \"PROVIDERS_RS_TEST_KEY\"\ndefault_model = \"mock-model\"\n",
        );
        std::env::set_var("LITE_HARNESS_PROVIDERS_FILE", &path);
        std::env::remove_var("LITE_HARNESS_PROVIDER");
        std::env::set_var("PROVIDERS_RS_TEST_KEY", "sk-test");

        let resolved = resolve_default_provider().unwrap().unwrap();
        assert_eq!(resolved.name, "mock");
        assert_eq!(resolved.model, "mock-model");
    }

    #[test]
    fn provider_env_var_overrides_which_named_provider_is_used() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = write_providers_toml(
            dir.path(),
            "default = \"mock\"\n\n\
             [[provider]]\nname = \"mock\"\nprotocol = \"open-ai-compatible\"\n\
             base_url = \"http://127.0.0.1:1\"\napi_key_env = \"PROVIDERS_RS_TEST_KEY\"\ndefault_model = \"mock-model\"\n\n\
             [[provider]]\nname = \"other\"\nprotocol = \"open-ai-compatible\"\n\
             base_url = \"http://127.0.0.1:2\"\napi_key_env = \"PROVIDERS_RS_TEST_KEY\"\ndefault_model = \"other-model\"\n",
        );
        std::env::set_var("LITE_HARNESS_PROVIDERS_FILE", &path);
        std::env::set_var("LITE_HARNESS_PROVIDER", "other");
        std::env::set_var("PROVIDERS_RS_TEST_KEY", "sk-test");

        let resolved = resolve_default_provider().unwrap().unwrap();
        assert_eq!(resolved.name, "other");
        assert_eq!(resolved.model, "other-model");
    }

    #[test]
    fn an_unknown_named_provider_is_a_clear_error() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = write_providers_toml(
            dir.path(),
            "default = \"mock\"\n\n[[provider]]\nname = \"mock\"\nprotocol = \"open-ai-compatible\"\n\
             base_url = \"http://127.0.0.1:1\"\napi_key_env = \"PROVIDERS_RS_TEST_KEY\"\ndefault_model = \"mock-model\"\n",
        );
        std::env::set_var("LITE_HARNESS_PROVIDERS_FILE", &path);
        std::env::set_var("LITE_HARNESS_PROVIDER", "nonexistent");

        assert!(resolve_default_provider().is_err());
    }

    #[test]
    fn malformed_toml_is_a_clear_error() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = write_providers_toml(dir.path(), "this is not valid toml [[[");
        std::env::set_var("LITE_HARNESS_PROVIDERS_FILE", &path);
        std::env::remove_var("LITE_HARNESS_PROVIDER");

        assert!(resolve_default_provider().is_err());
    }

    #[test]
    fn missing_api_key_env_var_surfaces_as_an_error() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = write_providers_toml(
            dir.path(),
            "default = \"mock\"\n\n[[provider]]\nname = \"mock\"\nprotocol = \"open-ai-compatible\"\n\
             base_url = \"http://127.0.0.1:1\"\napi_key_env = \"PROVIDERS_RS_TEST_KEY_DEFINITELY_UNSET\"\n\
             default_model = \"mock-model\"\n",
        );
        std::env::set_var("LITE_HARNESS_PROVIDERS_FILE", &path);
        std::env::remove_var("LITE_HARNESS_PROVIDER");
        std::env::remove_var("PROVIDERS_RS_TEST_KEY_DEFINITELY_UNSET");

        assert!(resolve_default_provider().is_err());
    }
}
