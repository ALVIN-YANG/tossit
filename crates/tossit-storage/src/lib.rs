use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;
use thiserror::Error;

pub const DATABASE_FILE_NAME: &str = "tossit.sqlite3";
pub const LEGACY_NETWORK_ID: &str = "legacy";
const SCHEMA_VERSION: i64 = 2;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TrustState {
    #[default]
    Discovered,
    Trusted,
    Blocked,
}

impl TrustState {
    fn as_database_value(self) -> &'static str {
        match self {
            Self::Discovered => "discovered",
            Self::Trusted => "trusted",
            Self::Blocked => "blocked",
        }
    }

    fn from_database_value(value: &str) -> Result<Self, StorageError> {
        match value {
            "discovered" => Ok(Self::Discovered),
            "trusted" => Ok(Self::Trusted),
            "blocked" => Ok(Self::Blocked),
            _ => Err(StorageError::InvalidData(format!(
                "unknown peer trust state {value:?}"
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredPeer {
    pub peer_id: String,
    pub display_id: String,
    pub alias: String,
    pub public_key: String,
    pub certificate_fingerprint: String,
    pub last_endpoint: Option<String>,
    pub last_seen_unix_ms: u64,
    pub trust_state: TrustState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredMessage {
    pub message_id: String,
    pub network_id: String,
    pub conversation_id: String,
    pub peer_id: String,
    pub direction: String,
    pub delivery: String,
    pub content_json: String,
    pub created_at_unix_ms: u64,
    pub is_read: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredNetworkSpace {
    pub network_id: String,
    pub display_name: String,
    pub first_used_unix_ms: u64,
    pub last_used_unix_ms: u64,
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("SQLite failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("stored data is invalid: {0}")]
    InvalidData(String),
    #[error("device identity changed for peer {0}")]
    IdentityChanged(String),
    #[error("peer {0} is not known")]
    UnknownPeer(String),
}

#[derive(Debug)]
pub struct Store {
    connection: Mutex<Connection>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                StorageError::InvalidData(format!(
                    "database directory could not be created: {error}"
                ))
            })?;
        }
        let connection = Connection::open(path)?;
        Self::from_connection(connection)
    }

    pub fn open_in_memory() -> Result<Self, StorageError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(mut connection: Connection) -> Result<Self, StorageError> {
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;\n\
             PRAGMA journal_mode = WAL;\n\
             PRAGMA synchronous = NORMAL;",
        )?;
        migrate(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn remember_peer(&self, peer: &StoredPeer) -> Result<TrustState, StorageError> {
        let connection = self.connection.lock().expect("storage connection lock");
        let existing = connection
            .query_row(
                "SELECT public_key, trust_state FROM peers WHERE peer_id = ?1",
                [&peer.peer_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        let trust_state = if let Some((public_key, trust_state)) = existing {
            if public_key != peer.public_key {
                return Err(StorageError::IdentityChanged(peer.peer_id.clone()));
            }
            TrustState::from_database_value(&trust_state)?
        } else {
            peer.trust_state
        };
        connection.execute(
            "INSERT INTO peers (\n\
                 peer_id, display_id, alias, public_key, certificate_fingerprint,\n\
                 last_endpoint, last_seen_unix_ms, trust_state\n\
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)\n\
             ON CONFLICT(peer_id) DO UPDATE SET\n\
                 display_id = excluded.display_id,\n\
                 alias = excluded.alias,\n\
                 certificate_fingerprint = CASE\n\
                   WHEN excluded.certificate_fingerprint = '' THEN peers.certificate_fingerprint\n\
                   ELSE excluded.certificate_fingerprint\n\
                 END,\n\
                 last_endpoint = COALESCE(excluded.last_endpoint, peers.last_endpoint),\n\
                 last_seen_unix_ms = MAX(peers.last_seen_unix_ms, excluded.last_seen_unix_ms)",
            params![
                peer.peer_id,
                peer.display_id,
                peer.alias,
                peer.public_key,
                peer.certificate_fingerprint,
                peer.last_endpoint,
                to_i64(peer.last_seen_unix_ms, "peer last-seen timestamp")?,
                trust_state.as_database_value(),
            ],
        )?;
        Ok(trust_state)
    }

    pub fn set_trust_state(
        &self,
        peer_id: &str,
        expected_public_key: &str,
        trust_state: TrustState,
    ) -> Result<(), StorageError> {
        let connection = self.connection.lock().expect("storage connection lock");
        let changed = connection.execute(
            "UPDATE peers SET trust_state = ?1 WHERE peer_id = ?2 AND public_key = ?3",
            params![
                trust_state.as_database_value(),
                peer_id,
                expected_public_key
            ],
        )?;
        if changed == 1 {
            return Ok(());
        }
        let known_key = connection
            .query_row(
                "SELECT public_key FROM peers WHERE peer_id = ?1",
                [peer_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        match known_key {
            Some(_) => Err(StorageError::IdentityChanged(peer_id.to_owned())),
            None => Err(StorageError::UnknownPeer(peer_id.to_owned())),
        }
    }

    pub fn load_peers(&self) -> Result<Vec<StoredPeer>, StorageError> {
        let connection = self.connection.lock().expect("storage connection lock");
        let mut statement = connection.prepare(
            "SELECT peer_id, display_id, alias, public_key, certificate_fingerprint,\n\
                    last_endpoint, last_seen_unix_ms, trust_state\n\
             FROM peers ORDER BY last_seen_unix_ms DESC, peer_id ASC",
        )?;
        let rows = statement.query_map([], |row| {
            let last_seen = row.get::<_, i64>(6)?;
            let trust = row.get::<_, String>(7)?;
            Ok((
                StoredPeer {
                    peer_id: row.get(0)?,
                    display_id: row.get(1)?,
                    alias: row.get(2)?,
                    public_key: row.get(3)?,
                    certificate_fingerprint: row.get(4)?,
                    last_endpoint: row.get(5)?,
                    last_seen_unix_ms: last_seen.try_into().unwrap_or_default(),
                    trust_state: TrustState::Discovered,
                },
                trust,
            ))
        })?;
        rows.map(|row| {
            let (mut peer, trust) = row?;
            peer.trust_state = TrustState::from_database_value(&trust)?;
            Ok(peer)
        })
        .collect()
    }

    pub fn save_message(&self, message: &StoredMessage) -> Result<(), StorageError> {
        let connection = self.connection.lock().expect("storage connection lock");
        connection.execute(
            "INSERT INTO messages (\n\
                 message_id, network_id, conversation_id, peer_id, direction, delivery, content_json,\n\
                 created_at_unix_ms, is_read\n\
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)\n\
             ON CONFLICT(message_id) DO UPDATE SET\n\
                 delivery = excluded.delivery,\n\
                 content_json = excluded.content_json,\n\
                 is_read = MAX(messages.is_read, excluded.is_read)",
            params![
                message.message_id,
                message.network_id,
                message.conversation_id,
                message.peer_id,
                message.direction,
                message.delivery,
                message.content_json,
                to_i64(message.created_at_unix_ms, "message timestamp")?,
                i64::from(message.is_read),
            ],
        )?;
        Ok(())
    }

    pub fn load_recent_messages(&self, limit: usize) -> Result<Vec<StoredMessage>, StorageError> {
        let limit = i64::try_from(limit)
            .map_err(|_| StorageError::InvalidData("message limit is too large".to_owned()))?;
        let connection = self.connection.lock().expect("storage connection lock");
        let mut statement = connection.prepare(
            "SELECT message_id, network_id, conversation_id, peer_id, direction, delivery, content_json,\n\
                    created_at_unix_ms, is_read\n\
             FROM (\n\
               SELECT * FROM messages\n\
               ORDER BY created_at_unix_ms DESC, message_id DESC LIMIT ?1\n\
             ) ORDER BY created_at_unix_ms ASC, message_id ASC",
        )?;
        let rows = statement.query_map([limit], stored_message_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn load_all_messages(&self) -> Result<Vec<StoredMessage>, StorageError> {
        let connection = self.connection.lock().expect("storage connection lock");
        let mut statement = connection.prepare(
            "SELECT message_id, network_id, conversation_id, peer_id, direction, delivery, content_json,
                    created_at_unix_ms, is_read
             FROM messages ORDER BY created_at_unix_ms ASC, message_id ASC",
        )?;
        let rows = statement.query_map([], stored_message_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn load_conversation_messages_before(
        &self,
        network_id: &str,
        peer_id: &str,
        before_created_at_unix_ms: u64,
        before_message_id: &str,
        limit: usize,
    ) -> Result<Vec<StoredMessage>, StorageError> {
        let limit = i64::try_from(limit)
            .map_err(|_| StorageError::InvalidData("message limit is too large".to_owned()))?;
        let before_created_at_unix_ms = to_i64(before_created_at_unix_ms, "message cursor")?;
        let connection = self.connection.lock().expect("storage connection lock");
        let mut statement = connection.prepare(
            "SELECT message_id, network_id, conversation_id, peer_id, direction, delivery, content_json,
                    created_at_unix_ms, is_read
             FROM (
               SELECT * FROM messages
               WHERE network_id = ?1 AND peer_id = ?2
                 AND (created_at_unix_ms < ?3 OR (created_at_unix_ms = ?3 AND message_id < ?4))
               ORDER BY created_at_unix_ms DESC, message_id DESC LIMIT ?5
             ) ORDER BY created_at_unix_ms ASC, message_id ASC",
        )?;
        let rows = statement.query_map(
            params![
                network_id,
                peer_id,
                before_created_at_unix_ms,
                before_message_id,
                limit
            ],
            stored_message_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn remember_network_space(&self, network: &StoredNetworkSpace) -> Result<(), StorageError> {
        let connection = self.connection.lock().expect("storage connection lock");
        connection.execute(
            "INSERT INTO network_spaces (\n\
                 network_id, display_name, first_used_unix_ms, last_used_unix_ms\n\
             ) VALUES (?1, ?2, ?3, ?4)\n\
             ON CONFLICT(network_id) DO UPDATE SET\n\
                 display_name = excluded.display_name,\n\
                 first_used_unix_ms = MIN(network_spaces.first_used_unix_ms, excluded.first_used_unix_ms),\n\
                 last_used_unix_ms = MAX(network_spaces.last_used_unix_ms, excluded.last_used_unix_ms)",
            params![
                network.network_id,
                network.display_name,
                to_i64(network.first_used_unix_ms, "network first-used timestamp")?,
                to_i64(network.last_used_unix_ms, "network last-used timestamp")?,
            ],
        )?;
        Ok(())
    }

    pub fn load_network_spaces(&self) -> Result<Vec<StoredNetworkSpace>, StorageError> {
        let connection = self.connection.lock().expect("storage connection lock");
        let mut statement = connection.prepare(
            "SELECT network_id, display_name, first_used_unix_ms, last_used_unix_ms\n\
             FROM network_spaces ORDER BY last_used_unix_ms DESC, network_id ASC",
        )?;
        let rows = statement.query_map([], |row| {
            let first_used = row.get::<_, i64>(2)?;
            let last_used = row.get::<_, i64>(3)?;
            Ok(StoredNetworkSpace {
                network_id: row.get(0)?,
                display_name: row.get(1)?,
                first_used_unix_ms: first_used.try_into().unwrap_or_default(),
                last_used_unix_ms: last_used.try_into().unwrap_or_default(),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn mark_peer_read(&self, peer_id: &str, network_id: &str) -> Result<(), StorageError> {
        let connection = self.connection.lock().expect("storage connection lock");
        connection.execute(
            "UPDATE messages SET is_read = 1\n\
             WHERE peer_id = ?1 AND network_id = ?2 AND direction = 'incoming'",
            params![peer_id, network_id],
        )?;
        Ok(())
    }
}

fn stored_message_from_row(row: &Row<'_>) -> rusqlite::Result<StoredMessage> {
    let created_at = row.get::<_, i64>(7)?;
    Ok(StoredMessage {
        message_id: row.get(0)?,
        network_id: row.get(1)?,
        conversation_id: row.get(2)?,
        peer_id: row.get(3)?,
        direction: row.get(4)?,
        delivery: row.get(5)?,
        content_json: row.get(6)?,
        created_at_unix_ms: created_at.try_into().unwrap_or_default(),
        is_read: row.get::<_, i64>(8)? != 0,
    })
}

fn migrate(connection: &mut Connection) -> Result<(), StorageError> {
    let version = connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?;
    if version > SCHEMA_VERSION {
        return Err(StorageError::InvalidData(format!(
            "database schema {version} is newer than supported schema {SCHEMA_VERSION}"
        )));
    }
    if version == 0 {
        let transaction = connection.transaction()?;
        transaction.execute_batch(
            "CREATE TABLE peers (\n\
               peer_id TEXT PRIMARY KEY NOT NULL,\n\
               display_id TEXT NOT NULL,\n\
               alias TEXT NOT NULL,\n\
               public_key TEXT NOT NULL,\n\
               certificate_fingerprint TEXT NOT NULL,\n\
               last_endpoint TEXT,\n\
               last_seen_unix_ms INTEGER NOT NULL CHECK(last_seen_unix_ms >= 0),\n\
               trust_state TEXT NOT NULL CHECK(trust_state IN ('discovered', 'trusted', 'blocked'))\n\
             );\n\
             CREATE TABLE messages (\n\
               message_id TEXT PRIMARY KEY NOT NULL,\n\
               network_id TEXT NOT NULL,\n\
               conversation_id TEXT NOT NULL,\n\
               peer_id TEXT NOT NULL REFERENCES peers(peer_id) ON DELETE CASCADE,\n\
               direction TEXT NOT NULL CHECK(direction IN ('incoming', 'outgoing')),\n\
               delivery TEXT NOT NULL CHECK(delivery IN ('received', 'receiving', 'sending', 'delivered', 'failed')),\n\
               content_json TEXT NOT NULL,\n\
               created_at_unix_ms INTEGER NOT NULL CHECK(created_at_unix_ms >= 0),\n\
               is_read INTEGER NOT NULL CHECK(is_read IN (0, 1))\n\
             );\n\
             CREATE TABLE network_spaces (\n\
               network_id TEXT PRIMARY KEY NOT NULL,\n\
               display_name TEXT NOT NULL,\n\
               first_used_unix_ms INTEGER NOT NULL CHECK(first_used_unix_ms >= 0),\n\
               last_used_unix_ms INTEGER NOT NULL CHECK(last_used_unix_ms >= 0)\n\
             );\n\
             CREATE INDEX messages_peer_time ON messages(peer_id, created_at_unix_ms DESC);\n\
             CREATE INDEX messages_network_peer_time ON messages(network_id, peer_id, created_at_unix_ms DESC);\n\
             CREATE INDEX messages_conversation_time ON messages(conversation_id, created_at_unix_ms DESC);\n\
             PRAGMA user_version = 2;",
        )?;
        transaction.commit()?;
    }
    if version == 1 {
        let transaction = connection.transaction()?;
        transaction.execute_batch(
            "CREATE TABLE network_spaces (\n\
               network_id TEXT PRIMARY KEY NOT NULL,\n\
               display_name TEXT NOT NULL,\n\
               first_used_unix_ms INTEGER NOT NULL CHECK(first_used_unix_ms >= 0),\n\
               last_used_unix_ms INTEGER NOT NULL CHECK(last_used_unix_ms >= 0)\n\
             );\n\
             ALTER TABLE messages ADD COLUMN network_id TEXT NOT NULL DEFAULT 'legacy';\n\
             INSERT INTO network_spaces (network_id, display_name, first_used_unix_ms, last_used_unix_ms)\n\
             SELECT 'legacy', '以前的局域网', MIN(created_at_unix_ms), MAX(created_at_unix_ms)\n\
             FROM messages HAVING COUNT(*) > 0;\n\
             CREATE INDEX messages_network_peer_time ON messages(network_id, peer_id, created_at_unix_ms DESC);\n\
             PRAGMA user_version = 2;",
        )?;
        transaction.commit()?;
    }
    Ok(())
}

fn to_i64(value: u64, field: &str) -> Result<i64, StorageError> {
    value
        .try_into()
        .map_err(|_| StorageError::InvalidData(format!("{field} is out of range")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> StoredPeer {
        StoredPeer {
            peer_id: "peer-1".to_owned(),
            display_id: "PEER-0001-TEST".to_owned(),
            alias: "TossIt PEER-0001-TEST".to_owned(),
            public_key: "public-key-1".to_owned(),
            certificate_fingerprint: "certificate-1".to_owned(),
            last_endpoint: Some("192.168.1.8:42100".to_owned()),
            last_seen_unix_ms: 42,
            trust_state: TrustState::Discovered,
        }
    }

    #[test]
    fn peer_trust_survives_rediscovery_and_restart() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let path = directory.path().join(DATABASE_FILE_NAME);
        {
            let store = Store::open(&path).expect("open store");
            let peer = peer();
            assert_eq!(
                store.remember_peer(&peer).expect("remember peer"),
                TrustState::Discovered
            );
            store
                .set_trust_state(&peer.peer_id, &peer.public_key, TrustState::Trusted)
                .expect("trust peer");
            let mut rediscovered = peer;
            rediscovered.last_seen_unix_ms = 84;
            assert_eq!(
                store
                    .remember_peer(&rediscovered)
                    .expect("remember rediscovered peer"),
                TrustState::Trusted
            );
        }
        let store = Store::open(path).expect("reopen store");
        let peers = store.load_peers().expect("load peers");
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].trust_state, TrustState::Trusted);
        assert_eq!(peers[0].last_seen_unix_ms, 84);
    }

    #[test]
    fn identity_change_cannot_reuse_a_trusted_peer_record() {
        let store = Store::open_in_memory().expect("open store");
        let peer = peer();
        store.remember_peer(&peer).expect("remember peer");
        store
            .set_trust_state(&peer.peer_id, &peer.public_key, TrustState::Trusted)
            .expect("trust peer");
        let mut changed = peer;
        changed.public_key = "different-key".to_owned();

        assert!(matches!(
            store.remember_peer(&changed),
            Err(StorageError::IdentityChanged(_))
        ));
    }

    #[test]
    fn messages_are_deduplicated_and_unread_state_is_durable() {
        let store = Store::open_in_memory().expect("open store");
        let peer = peer();
        store.remember_peer(&peer).expect("remember peer");
        let mut message = StoredMessage {
            message_id: "message-1".to_owned(),
            network_id: "network-1".to_owned(),
            conversation_id: "conversation-1".to_owned(),
            peer_id: peer.peer_id.clone(),
            direction: "incoming".to_owned(),
            delivery: "receiving".to_owned(),
            content_json: r#"{"type":"text","text":"hello"}"#.to_owned(),
            created_at_unix_ms: 100,
            is_read: false,
        };
        store.save_message(&message).expect("save message");
        message.delivery = "received".to_owned();
        store.save_message(&message).expect("update message");
        let loaded = store.load_recent_messages(100).expect("load messages");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].delivery, "received");
        assert!(!loaded[0].is_read);

        store
            .mark_peer_read(&peer.peer_id, &message.network_id)
            .expect("mark read");
        assert!(store.load_recent_messages(100).expect("reload")[0].is_read);
    }

    #[test]
    fn conversation_history_pages_backwards_with_a_stable_cursor() {
        let store = Store::open_in_memory().expect("open store");
        let peer = peer();
        store.remember_peer(&peer).expect("remember peer");
        for (message_id, created_at_unix_ms) in
            [("message-a", 100), ("message-b", 100), ("message-c", 200)]
        {
            store
                .save_message(&StoredMessage {
                    message_id: message_id.to_owned(),
                    network_id: "network-1".to_owned(),
                    conversation_id: "conversation-1".to_owned(),
                    peer_id: peer.peer_id.clone(),
                    direction: "incoming".to_owned(),
                    delivery: "received".to_owned(),
                    content_json: format!(r#"{{"type":"text","text":"{message_id}"}}"#),
                    created_at_unix_ms,
                    is_read: true,
                })
                .expect("save message");
        }

        let page = store
            .load_conversation_messages_before("network-1", &peer.peer_id, 200, "message-c", 2)
            .expect("load older page");
        assert_eq!(
            page.iter()
                .map(|message| message.message_id.as_str())
                .collect::<Vec<_>>(),
            vec!["message-a", "message-b"]
        );
        assert_eq!(store.load_all_messages().expect("load all").len(), 3);
    }

    #[test]
    fn network_spaces_are_created_only_when_explicitly_remembered() {
        let store = Store::open_in_memory().expect("open store");
        assert!(store.load_network_spaces().expect("load empty").is_empty());
        let network = StoredNetworkSpace {
            network_id: "network-1".to_owned(),
            display_name: "Home Wi-Fi".to_owned(),
            first_used_unix_ms: 100,
            last_used_unix_ms: 100,
        };
        store
            .remember_network_space(&network)
            .expect("remember network");
        let mut used_again = network.clone();
        used_again.last_used_unix_ms = 200;
        store
            .remember_network_space(&used_again)
            .expect("update network");
        let spaces = store.load_network_spaces().expect("load spaces");
        assert_eq!(spaces.len(), 1);
        assert_eq!(spaces[0].last_used_unix_ms, 200);
    }

    #[test]
    fn version_one_messages_migrate_into_one_legacy_network_space() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let path = directory.path().join(DATABASE_FILE_NAME);
        {
            let connection = Connection::open(&path).expect("open version one database");
            connection
                .execute_batch(
                    "CREATE TABLE peers (
                       peer_id TEXT PRIMARY KEY NOT NULL,
                       display_id TEXT NOT NULL,
                       alias TEXT NOT NULL,
                       public_key TEXT NOT NULL,
                       certificate_fingerprint TEXT NOT NULL,
                       last_endpoint TEXT,
                       last_seen_unix_ms INTEGER NOT NULL,
                       trust_state TEXT NOT NULL
                     );
                     CREATE TABLE messages (
                       message_id TEXT PRIMARY KEY NOT NULL,
                       conversation_id TEXT NOT NULL,
                       peer_id TEXT NOT NULL REFERENCES peers(peer_id) ON DELETE CASCADE,
                       direction TEXT NOT NULL,
                       delivery TEXT NOT NULL,
                       content_json TEXT NOT NULL,
                       created_at_unix_ms INTEGER NOT NULL,
                       is_read INTEGER NOT NULL
                     );
                     CREATE INDEX messages_peer_time ON messages(peer_id, created_at_unix_ms DESC);
                     CREATE INDEX messages_conversation_time ON messages(conversation_id, created_at_unix_ms DESC);
                     INSERT INTO peers VALUES (
                       'peer-1', 'PEER-0001-TEST', 'TossIt PEER-0001-TEST', 'public-key-1',
                       'certificate-1', NULL, 42, 'trusted'
                     );
                     INSERT INTO messages VALUES (
                       'old-message', 'old-conversation', 'peer-1', 'incoming', 'received',
                       '{\"type\":\"text\",\"text\":\"before migration\"}', 123, 0
                     );
                     PRAGMA user_version = 1;",
                )
                .expect("create version one schema");
        }

        let store = Store::open(&path).expect("migrate store");
        let messages = store
            .load_recent_messages(10)
            .expect("load migrated messages");
        let spaces = store.load_network_spaces().expect("load migrated networks");

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].network_id, LEGACY_NETWORK_ID);
        assert_eq!(spaces.len(), 1);
        assert_eq!(spaces[0].network_id, LEGACY_NETWORK_ID);
        assert_eq!(spaces[0].display_name, "以前的局域网");
        assert_eq!(spaces[0].first_used_unix_ms, 123);
        assert_eq!(spaces[0].last_used_unix_ms, 123);
    }
}
