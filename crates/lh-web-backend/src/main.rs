//! `lite-harness-web` binary entry point -- thin startup wiring only. All
//! actual routing/bridging logic lives in this crate's lib (`src/lib.rs`).

use std::net::SocketAddr;
use std::sync::Arc;

use lh_web_backend::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let sock_path = lh_protocol::default_socket_path(&cwd);

    let static_dir = std::env::var("LITE_HARNESS_WEB_STATIC_DIR")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../lh-web-ui").to_string());
    let port: u16 = std::env::var("LITE_HARNESS_WEB_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8787);

    let state = Arc::new(AppState { sock_path, cwd: cwd.to_string_lossy().to_string() });
    let app = lh_web_backend::build_router(state, &static_dir);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    eprintln!(
        "lite-harness-web listening on http://{addr} (workspace: {}, static: {static_dir})",
        cwd.display()
    );
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            sigterm.recv().await;
            eprintln!("received SIGTERM, shutting down");
        })
        .await?;
    Ok(())
}
