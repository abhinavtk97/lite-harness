//! `SessionStore` trait and a SQLite-backed implementation (architecture §3.3).
//!
//! Kept deliberately simple for the Phase 1 skeleton: a single
//! `Arc<Mutex<rusqlite::Connection>>` with blocking calls made from async
//! methods. Fine for a local, single-daemon-per-workspace process; a real
//! connection pool is a later optimization behind the same trait, not a
//! schema/API change.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use lh_event::{Event, SessionDriver, SessionId};
use tokio::sync::{broadcast, Mutex};

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

/// A subtree of the session tree, rooted at some session (architecture §3.2).
#[derive(Debug, Clone)]
pub struct SessionTreeNode {
    pub session_id: SessionId,
    pub driver: Option<SessionDriver>,
    pub children: Vec<SessionTreeNode>,
}

#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Appends `event`, assigning it the next `seq` for its session.
    /// Returns the assigned seq. `event.seq` as passed in is ignored.
    async fn append(&self, event: Event) -> Result<u64>;

    /// All events for `session_id` with `seq >= from_seq`, in order.
    async fn read_from(&self, session_id: SessionId, from_seq: u64) -> Result<Vec<Event>>;

    /// A live feed of newly-appended events across all sessions; callers
    /// filter by `session_id` (and, once subtree-subscribe lands, by tree
    /// membership).
    fn subscribe(&self) -> broadcast::Receiver<Event>;

    async fn session_tree(&self, root: SessionId) -> Result<SessionTreeNode>;

    /// Highest assigned seq for a session, or 0 if it has no events yet.
    async fn latest_seq(&self, session_id: SessionId) -> Result<u64>;
}

pub struct SqliteSessionStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
    tx: broadcast::Sender<Event>,
}

impl SqliteSessionStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = rusqlite::Connection::open(path)?;
        Self::init(&conn)?;
        let (tx, _rx) = broadcast::channel(1024);
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            tx,
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = rusqlite::Connection::open_in_memory()?;
        Self::init(&conn)?;
        let (tx, _rx) = broadcast::channel(1024);
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            tx,
        })
    }

    fn init(conn: &rusqlite::Connection) -> Result<()> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS events (
                 session_id TEXT NOT NULL,
                 seq INTEGER NOT NULL,
                 parent_session_id TEXT,
                 data TEXT NOT NULL,
                 PRIMARY KEY (session_id, seq)
             );
             CREATE INDEX IF NOT EXISTS idx_events_session ON events(session_id);
             CREATE INDEX IF NOT EXISTS idx_events_parent ON events(parent_session_id);",
        )?;
        Ok(())
    }
}

#[async_trait]
impl SessionStore for SqliteSessionStore {
    async fn append(&self, mut event: Event) -> Result<u64> {
        let conn = self.conn.lock().await;
        let session_id = event.session_id.to_string();
        let parent_id = event.parent_session_id.map(|p| p.to_string());

        let next_seq: i64 = conn.query_row(
            "SELECT COALESCE(MAX(seq), -1) + 1 FROM events WHERE session_id = ?1",
            [&session_id],
            |row| row.get(0),
        )?;
        event.seq = next_seq as u64;

        let data = serde_json::to_string(&event)?;
        conn.execute(
            "INSERT INTO events (session_id, seq, parent_session_id, data) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![session_id, next_seq, parent_id, data],
        )?;

        // Best-effort fan-out; no active subscribers is not an error.
        let _ = self.tx.send(event);

        Ok(next_seq as u64)
    }

    async fn read_from(&self, session_id: SessionId, from_seq: u64) -> Result<Vec<Event>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT data FROM events WHERE session_id = ?1 AND seq >= ?2 ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![session_id.to_string(), from_seq as i64],
            |row| row.get::<_, String>(0),
        )?;
        let mut events = Vec::new();
        for row in rows {
            events.push(serde_json::from_str(&row?)?);
        }
        Ok(events)
    }

    fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    async fn session_tree(&self, root: SessionId) -> Result<SessionTreeNode> {
        let conn = self.conn.lock().await;

        fn driver_of(
            conn: &rusqlite::Connection,
            session_id: SessionId,
        ) -> Result<Option<SessionDriver>> {
            // Convention: SessionDriverSet is always the first event (seq 0)
            // a daemon appends for a session.
            let data: Option<String> = conn
                .query_row(
                    "SELECT data FROM events WHERE session_id = ?1 AND seq = 0",
                    [session_id.to_string()],
                    |row| row.get(0),
                )
                .ok();
            let Some(data) = data else { return Ok(None) };
            let event: Event = serde_json::from_str(&data)?;
            if let lh_event::EventPayload::SessionDriverSet { driver } = event.payload {
                Ok(Some(driver))
            } else {
                Ok(None)
            }
        }

        fn build(
            conn: &rusqlite::Connection,
            session_id: SessionId,
        ) -> Result<SessionTreeNode> {
            let driver = driver_of(conn, session_id)?;

            let mut stmt = conn.prepare(
                "SELECT DISTINCT session_id FROM events WHERE parent_session_id = ?1",
            )?;
            let child_ids: Vec<String> = stmt
                .query_map([session_id.to_string()], |row| row.get(0))?
                .collect::<std::result::Result<_, _>>()?;

            let mut children = Vec::new();
            for child_id in child_ids {
                let child_id: SessionId = child_id.parse().map_err(|_| {
                    rusqlite::Error::InvalidColumnType(
                        0,
                        "session_id".into(),
                        rusqlite::types::Type::Text,
                    )
                })?;
                children.push(build(conn, child_id)?);
            }

            Ok(SessionTreeNode {
                session_id,
                driver,
                children,
            })
        }

        build(&conn, root)
    }

    async fn latest_seq(&self, session_id: SessionId) -> Result<u64> {
        let conn = self.conn.lock().await;
        let seq: i64 = conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE session_id = ?1",
            [session_id.to_string()],
            |row| row.get(0),
        )?;
        Ok(seq as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lh_event::{Actor, EventPayload, SessionDriver};

    #[tokio::test]
    async fn append_assigns_monotonic_seq_and_read_from_returns_in_order() {
        let store = SqliteSessionStore::open_in_memory().unwrap();
        let session_id = SessionId::now_v7();

        let e0 = Event::new(
            session_id,
            None,
            Actor::System,
            EventPayload::SessionDriverSet {
                driver: SessionDriver::Native,
            },
        );
        let e1 = Event::new(
            session_id,
            None,
            Actor::User,
            EventPayload::UserMessage {
                content: vec![lh_event::ContentBlock::text("hi")],
            },
        );

        assert_eq!(store.append(e0).await.unwrap(), 0);
        assert_eq!(store.append(e1).await.unwrap(), 1);
        assert_eq!(store.latest_seq(session_id).await.unwrap(), 1);

        let events = store.read_from(session_id, 0).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].seq, 0);
        assert_eq!(events[1].seq, 1);
    }

    #[tokio::test]
    async fn session_tree_includes_children() {
        let store = SqliteSessionStore::open_in_memory().unwrap();
        let root = SessionId::now_v7();
        let child = SessionId::now_v7();

        store
            .append(Event::new(
                root,
                None,
                Actor::System,
                EventPayload::SessionDriverSet {
                    driver: SessionDriver::Native,
                },
            ))
            .await
            .unwrap();
        store
            .append(Event::new(
                child,
                Some(root),
                Actor::System,
                EventPayload::SessionDriverSet {
                    driver: SessionDriver::Native,
                },
            ))
            .await
            .unwrap();

        let tree = store.session_tree(root).await.unwrap();
        assert_eq!(tree.session_id, root);
        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].session_id, child);
    }
}
