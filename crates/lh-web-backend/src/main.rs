//! `lite-harness-web` (architecture §11 phase 7). A *pure* Harness
//! Protocol client, structurally: unlike `lh-cli`, this crate never
//! parses a single protocol message. Each browser WebSocket connection is
//! bridged, byte-for-byte, to its own Unix-socket connection to the
//! per-workspace daemon -- one JSON line in, one WebSocket text frame
//! out, and vice versa. All the actual protocol logic (what to send when,
//! how to render an event, how to answer a permission ask) lives in
//! `lh-web-ui`'s plain JS, exactly mirroring what `lh-cli`'s Rust already
//! does for a terminal. "Just write a UI, not extend the core" (§11) is
//! close to literal here: the core doesn't grow a single new concept for
//! this phase, this crate is a transport adapter.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tower_http::services::ServeDir;

struct AppState {
    sock_path: PathBuf,
    /// The workspace root this backend was started in, as a string --
    /// exposed via `/api/cwd` so the browser can fill in
    /// `SessionCreateParams.cwd`. This isn't Harness Protocol logic (no
    /// JSON-RPC message is parsed), just server-local metadata the browser
    /// has no other way to learn.
    cwd: String,
}

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

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/api/cwd", get(cwd_handler))
        .fallback_service(ServeDir::new(&static_dir))
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    eprintln!(
        "lite-harness-web listening on http://{addr} (workspace: {}, static: {static_dir})",
        cwd.display()
    );
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn cwd_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    state.cwd.clone()
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        match lh_protocol::connect_or_spawn(&state.sock_path).await {
            Ok(stream) => {
                let (read_half, write_half) = stream.into_split();
                bridge(socket, BufReader::new(read_half), write_half).await;
            }
            Err(e) => {
                eprintln!("[lite-harness-web] failed to connect to daemon: {e:#}");
            }
        }
    })
}

/// The entire bridge: one JSON-RPC line per WebSocket text frame, in both
/// directions, until either side closes. No message is ever parsed or
/// even looked at -- `lh-web-ui`'s JS speaks the exact same Harness
/// Protocol JSON `lh-cli` speaks, this just moves the bytes.
async fn bridge(ws: WebSocket, mut uds_read: BufReader<OwnedReadHalf>, mut uds_write: OwnedWriteHalf) {
    let (mut ws_tx, mut ws_rx) = ws.split();

    let to_daemon = async {
        while let Some(Ok(msg)) = ws_rx.next().await {
            match msg {
                Message::Text(text) => {
                    let mut line = text.to_string();
                    line.push('\n');
                    if uds_write.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    };

    let to_browser = async {
        let mut line = String::new();
        loop {
            line.clear();
            match uds_read.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let trimmed = line.trim_end();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if ws_tx.send(Message::Text(trimmed.to_string().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    };

    tokio::select! {
        () = to_daemon => {}
        () = to_browser => {}
    }
}
