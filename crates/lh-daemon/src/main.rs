//! `lite-harnessd` binary entry point -- thin startup wiring only (parse
//! config, bind the socket, accept loop). All actual connection-handling
//! logic lives in this crate's lib (`src/lib.rs`); see its module doc for
//! why the split exists.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use lh_acp::registry::AgentsFile;
use lh_daemon::providers;
use lh_execution::{ExecutionPlane, LocalExecutionPlane};
use lh_ledger::{CostLedger, PricingTable, StoreBackedCostLedger};
use lh_permission::TomlPolicyStore;
use lh_protocol::default_socket_path;
use lh_store::{SessionStore, SqliteSessionStore};
use tokio::net::UnixListener;

#[tokio::main]
async fn main() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let sock_path = default_socket_path(&cwd);

    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if sock_path.exists() {
        std::fs::remove_file(&sock_path)?;
    }

    let listener = UnixListener::bind(&sock_path)?;
    eprintln!("lite-harnessd listening on {}", sock_path.display());

    let db_path = sock_path.with_extension("db");
    eprintln!("session store: {}", db_path.display());
    let store: Arc<dyn SessionStore> = Arc::new(SqliteSessionStore::open(&db_path)?);

    let resolved_provider = match providers::resolve_default_provider() {
        Ok(Some(p)) => {
            eprintln!("model provider ready (model={})", p.model);
            Some(p)
        }
        Ok(None) => {
            eprintln!(
                "no model provider configured -- session/prompt will fail until one is set \
                 (see LITE_HARNESS_PROVIDERS_FILE)"
            );
            None
        }
        Err(e) => {
            eprintln!("failed to load model provider config: {e:#}");
            None
        }
    };
    let resolved_provider = Arc::new(resolved_provider);

    let execution_plane: Arc<dyn ExecutionPlane> = Arc::new(LocalExecutionPlane::new(cwd.clone()).await?);
    let caps = execution_plane.describe();
    eprintln!(
        "execution plane ready (sandboxed={}, mechanism={}, network_restricted={})",
        caps.sandboxed, caps.mechanism, caps.network_restricted
    );

    // Policy layering (architecture §6): project-scoped rules live next to
    // the workspace, global-scoped rules follow the same
    // `~/.config/lite-harness/` convention as provider config (§13.2).
    // Loaded once here and shared (via Arc) across every connection so
    // concurrent sessions see a consistent in-memory view, not just an
    // eventually-consistent one via separate reloads of the same file.
    let project_policy_path = cwd.join(".lite-harness/policy.toml");
    let project_policy = match TomlPolicyStore::load(project_policy_path.clone()) {
        Ok(store) => Some(Arc::new(store)),
        Err(e) => {
            eprintln!("failed to load project policy store at {}: {e:#}", project_policy_path.display());
            None
        }
    };
    let global_policy = match std::env::var_os("HOME") {
        Some(home) => {
            let path = PathBuf::from(home).join(".config/lite-harness/policy.toml");
            match TomlPolicyStore::load(path.clone()) {
                Ok(store) => Some(Arc::new(store)),
                Err(e) => {
                    eprintln!("failed to load global policy store at {}: {e:#}", path.display());
                    None
                }
            }
        }
        None => None,
    };

    // Pricing (architecture §7/§13.3): built-in defaults for a few
    // well-known hosted models, overridable/extensible via the same
    // `~/.config/lite-harness/` convention -- an unpriced (e.g.
    // self-hosted) model just stays an honest `Unknown`, never a guess.
    let mut pricing = PricingTable::with_builtin_defaults();
    if let Some(home) = std::env::var_os("HOME") {
        let path = PathBuf::from(home).join(".config/lite-harness/pricing.toml");
        if let Err(e) = pricing.merge_overrides_from_file(&path) {
            eprintln!("failed to load pricing overrides from {}: {e:#}", path.display());
        }
    }
    let pricing = Arc::new(pricing);

    let cost_ledger: Arc<dyn CostLedger> = Arc::new(StoreBackedCostLedger::new(store.clone()));

    // Delegated-agent registry (architecture §5.2, §11 phase 4): mirrors
    // providers.toml/policy.toml's load-and-log-don't-crash convention --
    // no configured agents just means session/delegate errors per-request,
    // not a daemon startup failure.
    let agents_registry = match lh_acp::registry::agents_path() {
        Some(path) => match lh_acp::registry::load_agents_file(&path) {
            Ok(file) => {
                eprintln!("agent registry ready ({} adapter(s) from {})", file.agents.len(), path.display());
                file
            }
            Err(e) => {
                eprintln!("failed to load agent registry at {}: {e:#}", path.display());
                AgentsFile::default()
            }
        },
        None => AgentsFile::default(),
    };
    let agents_registry = Arc::new(agents_registry);

    // Graceful shutdown on SIGTERM (the standard signal a process manager
    // or `kill` without flags sends): stop accepting new connections and
    // return from main() normally rather than only ever going down via
    // SIGKILL. Previously this daemon had no way to stop cleanly at all --
    // in-flight requests on already-accepted connections are unaffected
    // (their tasks keep running independently of this loop), only new
    // connections stop being accepted.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _addr) = accepted?;
                let store = store.clone();
                let resolved_provider = resolved_provider.clone();
                let execution_plane = execution_plane.clone();
                let project_policy = project_policy.clone();
                let global_policy = global_policy.clone();
                let pricing = pricing.clone();
                let cost_ledger = cost_ledger.clone();
                let agents_registry = agents_registry.clone();
                tokio::spawn(async move {
                    if let Err(e) = lh_daemon::handle_connection(
                        stream,
                        store,
                        resolved_provider,
                        execution_plane,
                        project_policy,
                        global_policy,
                        pricing,
                        cost_ledger,
                        agents_registry,
                    )
                    .await
                    {
                        eprintln!("connection error: {e:#}");
                    }
                });
            }
            _ = sigterm.recv() => {
                eprintln!("received SIGTERM, shutting down");
                break;
            }
        }
    }

    Ok(())
}
