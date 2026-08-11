//! The daemon connection: a background task owns the read half and decodes
//! every incoming `Message` into a `ClientEvent` pushed onto an unbounded
//! channel; the main (ratatui) loop owns the write half and `tokio::select!`s
//! between this channel and terminal input. One reader task, one channel,
//! for the whole connection's lifetime -- no separate "handshake phase" vs
//! "interactive phase" split, so `initialize`/`session/create` complete
//! through the exact same `Response`-dispatch path the rest of the app uses
//! (see `App::handle_client_event`).

use std::path::Path;

use anyhow::Result;
use lh_protocol::{
    buffered, connect_or_spawn, methods, read_message, write_message, Message, PermissionAskParams,
    Request, RequestId, Response,
};
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::mpsc;

/// A decoded, already-classified message from the daemon -- the main loop
/// never sees the raw `Message` enum, only these four cases (mirrors the
/// exact set `lh-cli`'s own read loop distinguishes).
#[derive(Debug)]
pub enum ClientEvent {
    SessionEvent(lh_event::Event),
    PermissionAsk { request_id: RequestId, request: lh_event::PermissionRequest },
    Response(Response),
    /// EOF, or a decode error the reader can't recover from -- either way
    /// the connection is done.
    Closed,
}

pub struct DaemonClient {
    write_half: OwnedWriteHalf,
    next_id: RequestId,
}

impl DaemonClient {
    /// Connects (spawning the daemon if nothing is listening yet, same as
    /// every other Harness Protocol client) and starts the background
    /// reader. Returns the client half (for sending) and the event channel
    /// the caller's main loop should `select!` on.
    pub async fn connect(sock_path: &Path) -> Result<(Self, mpsc::UnboundedReceiver<ClientEvent>)> {
        let stream = connect_or_spawn(sock_path).await?;
        let (read_half, write_half) = stream.into_split();
        let mut reader = buffered(read_half);
        let (tx, rx) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            loop {
                let decoded = match read_message(&mut reader).await {
                    Ok(Some(msg)) => msg,
                    Ok(None) | Err(_) => {
                        let _ = tx.send(ClientEvent::Closed);
                        return;
                    }
                };
                let event = match decoded {
                    Message::Notification(n) if n.method == methods::EVENT => {
                        match serde_json::from_value(n.params) {
                            Ok(event) => ClientEvent::SessionEvent(event),
                            Err(_) => continue,
                        }
                    }
                    Message::Request(req) if req.method == methods::PERMISSION_ASK => {
                        match serde_json::from_value::<PermissionAskParams>(req.params) {
                            Ok(params) => ClientEvent::PermissionAsk { request_id: req.id, request: params.request },
                            Err(_) => continue,
                        }
                    }
                    Message::Response(resp) => ClientEvent::Response(resp),
                    // Anything else on this connection (a stray notification
                    // whose method we don't recognize, or a Request we have
                    // no handler for) is deliberately ignored rather than
                    // treated as fatal -- matches lh-cli's own tolerance for
                    // unexpected messages.
                    _ => continue,
                };
                if tx.send(event).is_err() {
                    return;
                }
            }
        });

        Ok((Self { write_half, next_id: 1 }, rx))
    }

    /// Writes a `Request` and returns its id; the caller correlates the
    /// eventual `ClientEvent::Response` by that id off the shared channel
    /// (see `App::handle_client_event`) rather than awaiting it here --
    /// awaiting here would block the whole event loop (no rendering, no
    /// key handling) for the duration of the round trip.
    pub async fn send_request(&mut self, method: &str, params: serde_json::Value) -> Result<RequestId> {
        let id = self.next_id;
        self.next_id += 1;
        write_message(&mut self.write_half, &Message::Request(Request::new(id, method, params))).await?;
        Ok(id)
    }

    pub async fn respond(&mut self, id: RequestId, result: serde_json::Value) -> Result<()> {
        write_message(&mut self.write_half, &Message::Response(Response::ok(id, result))).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixListener;

    use super::*;

    /// A fake "daemon" for these tests -- binds a real socket and hands the
    /// test a raw line-writer to the accepted connection, so tests can send
    /// exactly the (possibly malformed) bytes they want without going
    /// through a real `lite-harnessd` process.
    async fn fake_daemon() -> (std::path::PathBuf, tokio::sync::oneshot::Receiver<tokio::net::UnixStream>) {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("fake.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (stream, _addr) = listener.accept().await.unwrap();
            let _ = tx.send(stream);
            // Keep the tempdir alive for the socket's lifetime.
            std::mem::forget(dir);
        });
        (sock_path, rx)
    }

    #[tokio::test]
    async fn reports_closed_when_the_daemon_closes_the_connection() {
        let (sock_path, accepted) = fake_daemon().await;
        let (_client, mut events) = DaemonClient::connect(&sock_path).await.unwrap();

        let stream = accepted.await.unwrap();
        drop(stream); // daemon "closes" the connection

        assert!(matches!(events.recv().await, Some(ClientEvent::Closed)));
    }

    #[tokio::test]
    async fn an_unrecognized_notification_and_request_are_skipped_not_fatal() {
        let (sock_path, accepted) = fake_daemon().await;
        let (_client, mut events) = DaemonClient::connect(&sock_path).await.unwrap();
        let mut stream = accepted.await.unwrap();

        // A notification whose method isn't "event", and a request whose
        // method isn't "permission/ask" -- both fall through the `_ =>
        // continue` arm rather than being treated as `ClientEvent`s.
        stream.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"totally/unknown\",\"params\":{}}\n").await.unwrap();
        stream.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":99,\"method\":\"also/unknown\",\"params\":{}}\n").await.unwrap();
        // Something the reader *does* recognize, to prove it kept going
        // rather than getting stuck on the two lines above.
        stream.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n").await.unwrap();

        let event = events.recv().await.unwrap();
        assert!(matches!(event, ClientEvent::Response(resp) if resp.id == 1));
    }

    #[tokio::test]
    async fn malformed_event_and_permission_ask_params_are_skipped_not_fatal() {
        let (sock_path, accepted) = fake_daemon().await;
        let (_client, mut events) = DaemonClient::connect(&sock_path).await.unwrap();
        let mut stream = accepted.await.unwrap();

        // "event" notification whose params don't deserialize as
        // `lh_event::Event`, and a "permission/ask" request whose params
        // don't deserialize as `PermissionAskParams` -- both must be
        // skipped rather than crashing the reader task.
        stream.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"event\",\"params\":\"not an event\"}\n").await.unwrap();
        stream
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":-1,\"method\":\"permission/ask\",\"params\":\"not a request\"}\n")
            .await
            .unwrap();
        stream.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n").await.unwrap();

        let event = events.recv().await.unwrap();
        assert!(matches!(event, ClientEvent::Response(resp) if resp.id == 2));
    }
}
