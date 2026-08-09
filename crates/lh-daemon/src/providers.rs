//! Resolves which model provider `lite-harnessd` should use for the native
//! agent loop (architecture §13.2), from a TOML config file. Overridable
//! via env vars so this is testable without touching `$HOME` or needing a
//! real API key -- `LITE_HARNESS_PROVIDERS_FILE` points at the file,
//! `LITE_HARNESS_PROVIDER` picks a provider by name instead of the file's
//! `default`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use lh_model_provider::ModelProvider;

pub struct ResolvedProvider {
    pub provider: Arc<dyn ModelProvider>,
    pub model: String,
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
        model: cfg.default_model.clone(),
    }))
}
