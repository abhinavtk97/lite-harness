//! Model pricing (architecture §7, §13.3): turns a provider/model's token
//! counts into a dollar figure when a price is known, and produces an
//! honest `Unknown` when it isn't -- never a fabricated number. This
//! applies identically to a self-hosted/local model (§13's BYO base URL)
//! as to a well-known hosted one: no entry in the table means `Unknown`,
//! full stop.

use std::collections::HashMap;
use std::path::Path;

use lh_event::UsageConfidence;
use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, PricingError>;

#[derive(Debug, thiserror::Error)]
pub enum PricingError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse error: {0}")]
    Parse(#[from] toml::de::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelPrice {
    pub input_per_million: f64,
    pub output_per_million: f64,
}

/// `(provider name, model name) -> price`. Provider/model names are
/// matched exactly against `ModelProviderConfig.name` / `default_model`
/// (`lh-model-provider`, §13.2) -- whatever the operator called the
/// provider in `providers.toml` is what's looked up here.
#[derive(Debug, Clone, Default)]
pub struct PricingTable {
    prices: HashMap<(String, String), ModelPrice>,
}

impl PricingTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// A small, deliberately non-exhaustive starter table for a few
    /// well-known hosted models, so the mechanism is provably wired up
    /// end to end. Real deployments should maintain their own
    /// `pricing.toml` overrides (`with_overrides`) rather than rely on
    /// this staying current -- hosted-model pricing changes over time and
    /// this table is not a source of truth for it.
    pub fn with_builtin_defaults() -> Self {
        let mut table = Self::new();
        let defaults: &[(&str, &str, f64, f64)] = &[
            ("anthropic", "claude-opus-4", 15.0, 75.0),
            ("anthropic", "claude-sonnet-4", 3.0, 15.0),
            ("anthropic", "claude-haiku-4", 0.8, 4.0),
            ("openai", "gpt-4o", 2.5, 10.0),
            ("openai", "gpt-4o-mini", 0.15, 0.6),
        ];
        for (provider, model, input, output) in defaults {
            table.insert(provider, model, ModelPrice {
                input_per_million: *input,
                output_per_million: *output,
            });
        }
        table
    }

    pub fn insert(&mut self, provider: &str, model: &str, price: ModelPrice) {
        self.prices.insert((provider.to_string(), model.to_string()), price);
    }

    pub fn lookup(&self, provider: &str, model: &str) -> Option<ModelPrice> {
        self.prices.get(&(provider.to_string(), model.to_string())).copied()
    }

    /// Merges in overrides from a TOML file (operator-maintained,
    /// typically `~/.config/lite-harness/pricing.toml`), taking priority
    /// over any built-in entry with the same key. Missing file is not an
    /// error -- pricing overrides are optional.
    pub fn merge_overrides_from_file(&mut self, path: &Path) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let text = std::fs::read_to_string(path)?;
        let file: PricingFile = toml::from_str(&text)?;
        for entry in file.price {
            self.insert(&entry.provider, &entry.model, ModelPrice {
                input_per_million: entry.input_per_million,
                output_per_million: entry.output_per_million,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct PricingFile {
    #[serde(default)]
    price: Vec<PricingEntry>,
}

#[derive(Debug, Deserialize)]
struct PricingEntry {
    provider: String,
    model: String,
    input_per_million: f64,
    output_per_million: f64,
}

/// Prices one turn's usage. `Unknown` confidence -- and no dollar figure
/// -- whenever the provider didn't report token counts, or no price is on
/// file for this (provider, model) pair. This is the "honest gap" pattern
/// used throughout the ledger (architecture §7/§13.3): never guess.
pub fn price_usage(
    table: &PricingTable,
    provider: &str,
    model: &str,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
) -> (Option<f64>, UsageConfidence) {
    let (Some(input), Some(output)) = (input_tokens, output_tokens) else {
        return (None, UsageConfidence::Unknown);
    };
    let Some(price) = table.lookup(provider, model) else {
        return (None, UsageConfidence::Unknown);
    };
    let cost = (input as f64 / 1_000_000.0) * price.input_per_million
        + (output as f64 / 1_000_000.0) * price.output_per_million;
    (Some(cost), UsageConfidence::Exact)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_model_prices_exactly() {
        let table = PricingTable::with_builtin_defaults();
        let (cost, confidence) =
            price_usage(&table, "anthropic", "claude-sonnet-4", Some(1_000_000), Some(1_000_000));
        assert_eq!(cost, Some(3.0 + 15.0));
        assert_eq!(confidence, UsageConfidence::Exact);
    }

    #[test]
    fn unpriced_model_is_an_honest_unknown_not_a_guess() {
        let table = PricingTable::with_builtin_defaults();
        let (cost, confidence) = price_usage(&table, "local-llama", "llama3", Some(100), Some(50));
        assert_eq!(cost, None);
        assert_eq!(confidence, UsageConfidence::Unknown);
    }

    #[test]
    fn missing_token_counts_are_unknown_even_for_a_priced_model() {
        let table = PricingTable::with_builtin_defaults();
        let (cost, confidence) = price_usage(&table, "anthropic", "claude-sonnet-4", None, Some(50));
        assert_eq!(cost, None);
        assert_eq!(confidence, UsageConfidence::Unknown);
    }

    #[test]
    fn overrides_from_file_take_priority_and_add_new_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pricing.toml");
        std::fs::write(
            &path,
            r#"
[[price]]
provider = "anthropic"
model = "claude-sonnet-4"
input_per_million = 1.0
output_per_million = 2.0

[[price]]
provider = "local-llama"
model = "llama3"
input_per_million = 0.0
output_per_million = 0.0
"#,
        )
        .unwrap();

        let mut table = PricingTable::with_builtin_defaults();
        table.merge_overrides_from_file(&path).unwrap();

        assert_eq!(
            table.lookup("anthropic", "claude-sonnet-4"),
            Some(ModelPrice { input_per_million: 1.0, output_per_million: 2.0 })
        );
        assert_eq!(
            table.lookup("local-llama", "llama3"),
            Some(ModelPrice { input_per_million: 0.0, output_per_million: 0.0 })
        );
    }

    #[test]
    fn missing_override_file_is_not_an_error() {
        let mut table = PricingTable::with_builtin_defaults();
        table.merge_overrides_from_file(Path::new("/nonexistent/pricing.toml")).unwrap();
    }
}
