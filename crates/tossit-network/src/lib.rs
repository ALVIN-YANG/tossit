#[cfg(target_os = "ios")]
mod apple_bonjour;

#[cfg(not(target_os = "ios"))]
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls13_signature, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{
    version, ClientConfig, ClientConnection, DigitallySignedStruct, ServerConfig, ServerConnection,
    SignatureScheme, StreamOwned,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Write};
#[cfg(not(target_os = "ios"))]
use std::net::ToSocketAddrs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tossit_identity::{DeviceIdentity, DeviceIdentitySummary, IdentityError};
pub use tossit_protocol::AttachmentKind;
use tossit_protocol::{Envelope, Payload, MAX_ATTACHMENT_BYTES, MAX_TEXT_BYTES, PROTOCOL_VERSION};
pub use tossit_storage::TrustState;
use tossit_storage::{StorageError, Store, StoredMessage, StoredNetworkSpace, StoredPeer};
use uuid::Uuid;

pub const SERVICE_TYPE: &str = "_tossit._tcp.local.";

const FRAME_MAX_BYTES: usize = 128 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const TRANSFER_CHUNK_BYTES: usize = 64 * 1024;
const THUMBNAIL_MAX_EDGE: u32 = 960;
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const PENDING_DELIVERY_POLL_INTERVAL: Duration = Duration::from_secs(1);
const DELIVERY_RETRY_DELAY_MS: u64 = 5_000;
const CLIENT_PROOF_CONTEXT: &str = "tossit-client-proof-v2";
const SERVER_PROOF_CONTEXT: &str = "tossit-server-proof-v1";
const VERIFICATION_CODE_CONTEXT: &str = "tossit-verification-code-v1";
const MAX_LOADED_MESSAGES: usize = 2_000;
const MAX_AVATAR_BYTES: u64 = 512 * 1024;
const AVATAR_MEDIA_TYPE: &str = "image/jpeg";
const STORAGE_HEADROOM_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NearbyPeer {
    pub peer_id: String,
    pub display_id: String,
    pub alias: String,
    pub endpoint: Option<String>,
    pub is_online: bool,
    pub last_seen_unix_ms: u64,
    pub trust_state: TrustState,
    pub verification_code: String,
    pub unread_count: usize,
    pub avatar_hash: Option<String>,
    pub avatar_path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub message_id: String,
    pub network_id: String,
    pub conversation_id: String,
    pub peer_id: String,
    pub direction: MessageDirection,
    pub delivery: DeliveryState,
    pub content: ChatContent,
    pub created_at_unix_ms: u64,
    pub is_read: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveNetwork {
    pub network_id: String,
    pub display_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkSpace {
    pub network_id: String,
    pub display_name: String,
    pub first_used_unix_ms: u64,
    pub last_used_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ChatContent {
    Text { text: String },
    Attachment { attachment: ChatAttachment },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatAttachment {
    pub transfer_id: String,
    pub kind: AttachmentKind,
    pub file_name: String,
    pub media_type: String,
    pub byte_size: u64,
    pub transferred_bytes: u64,
    pub local_path: Option<String>,
    pub preview_path: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MessageDirection {
    Incoming,
    Outgoing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DeliveryState {
    Received,
    Receiving,
    Sending,
    Delivered,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkSnapshot {
    pub listening_port: u16,
    pub local_endpoints: Vec<String>,
    pub active_network: Option<ActiveNetwork>,
    pub network_spaces: Vec<NetworkSpace>,
    pub peers: Vec<NearbyPeer>,
    pub messages: Vec<ChatMessage>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryPage {
    pub snapshot: NetworkSnapshot,
    pub loaded: usize,
    pub has_more: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageSummary {
    pub received_file_count: usize,
    pub received_bytes: u64,
}

#[derive(Clone)]
pub struct NetworkNode {
    runtime: Arc<NodeRuntime>,
}

struct NodeRuntime {
    shared: Arc<Shared>,
    #[cfg(not(target_os = "ios"))]
    mdns: ServiceDaemon,
    #[cfg(not(target_os = "ios"))]
    service_fullname: RwLock<String>,
    #[cfg(target_os = "ios")]
    bonjour: apple_bonjour::SystemBonjour,
}

struct Shared {
    identity: Arc<DeviceIdentity>,
    identity_summary: DeviceIdentitySummary,
    nickname: RwLock<String>,
    avatar: RwLock<Option<LocalAvatar>>,
    certificate_fingerprint: String,
    server_config: Arc<ServerConfig>,
    listening_port: u16,
    attachment_dir: PathBuf,
    avatar_cache_dir: PathBuf,
    store: Arc<Store>,
    active_network: RwLock<Option<ActiveNetwork>>,
    state: RwLock<NodeState>,
    delivery_in_flight: Mutex<HashSet<String>>,
    cancelled_deliveries: Mutex<HashSet<String>>,
    delivery_retry_after: Mutex<HashMap<String, u64>>,
    sequence: AtomicU64,
    shutdown: AtomicBool,
}

#[derive(Clone)]
struct LocalAvatar {
    hash: String,
    contents: Arc<[u8]>,
}

#[derive(Default)]
struct NodeState {
    peers: HashMap<String, PeerState>,
    service_peers: HashMap<String, String>,
    network_spaces: Vec<NetworkSpace>,
    messages: Vec<ChatMessage>,
}

#[derive(Clone)]
struct PeerState {
    peer_id: String,
    display_id: String,
    alias: String,
    public_key: String,
    certificate_fingerprint: String,
    endpoint: Option<SocketAddr>,
    service_fullnames: HashSet<String>,
    last_seen_unix_ms: u64,
    trust_state: TrustState,
    avatar_hash: Option<String>,
    avatar_path: Option<PathBuf>,
}

impl PeerState {
    fn snapshot(&self, verification_code: String, unread_count: usize) -> NearbyPeer {
        NearbyPeer {
            peer_id: self.peer_id.clone(),
            display_id: self.display_id.clone(),
            alias: self.alias.clone(),
            endpoint: self.endpoint.map(|endpoint| endpoint.to_string()),
            is_online: self.endpoint.is_some(),
            last_seen_unix_ms: self.last_seen_unix_ms,
            trust_state: self.trust_state,
            verification_code,
            unread_count,
            avatar_hash: self.avatar_hash.clone(),
            avatar_path: self.avatar_path.as_deref().map(path_string),
        }
    }
}

#[derive(Debug, Error)]
pub enum NetworkError {
    #[error("device identity failed: {0}")]
    Identity(#[from] IdentityError),
    #[error("network I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("local discovery failed: {0}")]
    Discovery(String),
    #[error("TLS setup or connection failed: {0}")]
    Tls(String),
    #[error("wire data is invalid: {0}")]
    InvalidWire(String),
    #[error("nearby device is not currently reachable")]
    PeerOffline,
    #[error("nearby device identity did not match its advertisement")]
    PeerIdentityMismatch,
    #[error("message acknowledgement did not match")]
    InvalidAcknowledgement,
    #[error("attachment failed: {0}")]
    Attachment(String),
    #[error("传输已取消")]
    TransferCancelled,
    #[error("storage failed: {0}")]
    Storage(#[from] StorageError),
    #[error("请先在两台设备上确认校验码并信任对方")]
    PeerUntrusted,
    #[error("请连接 Wi-Fi 并允许 TossIt 识别当前网络后再发送")]
    NoActiveNetwork,
    #[error("对方设备拒绝连接：{0}")]
    PeerRejected(String),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum WireFrame {
    Challenge {
        protocol_version: u16,
        server_nonce: String,
        certificate_fingerprint: String,
    },
    IdentityProof {
        peer_id: String,
        display_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        nickname: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        avatar_hash: Option<String>,
        public_key: String,
        certificate_fingerprint: String,
        listening_port: u16,
        client_nonce: String,
        signature: String,
    },
    IdentityAccepted {
        peer_id: String,
        display_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        nickname: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        avatar_hash: Option<String>,
        public_key: String,
        signature: String,
    },
    Message {
        envelope: Envelope,
    },
    TransferOffer {
        envelope: Envelope,
    },
    TransferReady {
        transfer_id: String,
    },
    TransferDigest {
        transfer_id: String,
        sha256: String,
    },
    AvatarRequest {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cached_hash: Option<String>,
    },
    AvatarOffer {
        avatar_hash: String,
        media_type: String,
        byte_size: u64,
    },
    AvatarUnchanged {
        avatar_hash: String,
    },
    AvatarUnavailable,
    Rejected {
        reason: String,
    },
}

fn normalize_nickname(value: &str, display_id: &str) -> String {
    let nickname = value.trim();
    if nickname.is_empty()
        || nickname.chars().count() > 24
        || nickname.len() > 72
        || nickname.chars().any(char::is_control)
    {
        format!("TossIt {display_id}")
    } else {
        nickname.to_owned()
    }
}

fn normalize_avatar_hash(value: &str) -> Option<String> {
    let value = value.trim();
    (value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
}

fn local_avatar_from_path(path: &Path) -> Result<LocalAvatar, NetworkError> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_AVATAR_BYTES {
        return Err(NetworkError::InvalidWire(
            "local avatar file is invalid".to_owned(),
        ));
    }
    let contents = fs::read(path)?;
    Ok(LocalAvatar {
        hash: hex::encode(Sha256::digest(&contents)),
        contents: contents.into(),
    })
}

fn peer_avatar_file(shared: &Shared, peer_id: &str, hash: &str) -> PathBuf {
    shared
        .avatar_cache_dir
        .join(format!("{peer_id}-{hash}.jpg"))
}

fn cached_avatar_path(shared: &Shared, peer_id: &str, hash: &str) -> Option<PathBuf> {
    let path = peer_avatar_file(shared, peer_id, hash);
    path.is_file().then_some(path)
}

#[cfg(not(target_os = "ios"))]
fn discovery_service(
    identity: &DeviceIdentitySummary,
    nickname: &str,
    avatar_hash: Option<&str>,
    certificate_fingerprint: &str,
    listening_port: u16,
) -> Result<ServiceInfo, NetworkError> {
    let instance_name = format!("TossIt {}", identity.display_id);
    let hostname = format!(
        "tossit-{}.local.",
        identity.display_id.replace('-', "").to_lowercase()
    );
    let protocol_version = PROTOCOL_VERSION.to_string();
    let avatar_hash = avatar_hash.unwrap_or_default();
    let properties = [
        ("peer", identity.peer_id.as_str()),
        ("display", identity.display_id.as_str()),
        ("nickname", nickname),
        ("avatar", avatar_hash),
        ("key", identity.public_key.as_str()),
        ("cert", certificate_fingerprint),
        ("v", protocol_version.as_str()),
        ("caps", "text,attachment-v1,avatar-v1"),
    ];
    ServiceInfo::new(
        SERVICE_TYPE,
        &instance_name,
        &hostname,
        "",
        listening_port,
        &properties[..],
    )
    .map(|service| service.enable_addr_auto())
    .map_err(discovery_error)
}

impl NetworkNode {
    pub fn start(
        identity: Arc<DeviceIdentity>,
        nickname: String,
        attachment_dir: impl Into<PathBuf>,
        store: Store,
    ) -> Result<Self, NetworkError> {
        let attachment_dir = attachment_dir.into();
        fs::create_dir_all(attachment_dir.join("incoming"))?;
        fs::create_dir_all(attachment_dir.join("outgoing"))?;
        fs::create_dir_all(attachment_dir.join("previews"))?;
        let avatar_cache_dir = attachment_dir.join("avatars");
        fs::create_dir_all(&avatar_cache_dir)?;
        let tls_material = identity.tls_material()?;
        let provider = rustls::crypto::ring::default_provider();
        let server_config = ServerConfig::builder_with_provider(provider.into())
            .with_protocol_versions(&[&version::TLS13])
            .map_err(tls_error)?
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(tls_material.certificate_der)],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(tls_material.private_key_der)),
            )
            .map_err(tls_error)?;

        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        listener.set_nonblocking(true)?;
        let listening_port = listener.local_addr()?.port();
        let identity_summary = identity.summary();
        let nickname = normalize_nickname(&nickname, &identity_summary.display_id);
        let store = Arc::new(store);
        let state = load_persisted_state(&store)?;
        let shared = Arc::new(Shared {
            identity,
            identity_summary: identity_summary.clone(),
            nickname: RwLock::new(nickname.clone()),
            avatar: RwLock::new(None),
            certificate_fingerprint: tls_material.certificate_fingerprint.clone(),
            server_config: Arc::new(server_config),
            listening_port,
            attachment_dir,
            avatar_cache_dir,
            store,
            active_network: RwLock::new(None),
            state: RwLock::new(state),
            delivery_in_flight: Mutex::new(HashSet::new()),
            cancelled_deliveries: Mutex::new(HashSet::new()),
            delivery_retry_after: Mutex::new(HashMap::new()),
            sequence: AtomicU64::new(1),
            shutdown: AtomicBool::new(false),
        });

        #[cfg(not(target_os = "ios"))]
        let (mdns, service_fullname, browse_receiver) = {
            let mdns = ServiceDaemon::new().map_err(discovery_error)?;
            let service = discovery_service(
                &identity_summary,
                &nickname,
                None,
                &tls_material.certificate_fingerprint,
                listening_port,
            )?;
            let service_fullname = service.get_fullname().to_owned();
            mdns.register(service).map_err(discovery_error)?;
            let browse_receiver = mdns.browse(SERVICE_TYPE).map_err(discovery_error)?;
            (mdns, service_fullname, browse_receiver)
        };

        #[cfg(target_os = "ios")]
        let bonjour = apple_bonjour::SystemBonjour::start(Arc::clone(&shared))?;

        spawn_listener(Arc::clone(&shared), listener);
        spawn_pending_delivery_worker(Arc::clone(&shared));
        #[cfg(not(target_os = "ios"))]
        spawn_browser(Arc::clone(&shared), browse_receiver);

        Ok(Self {
            runtime: Arc::new(NodeRuntime {
                shared,
                #[cfg(not(target_os = "ios"))]
                mdns,
                #[cfg(not(target_os = "ios"))]
                service_fullname: RwLock::new(service_fullname),
                #[cfg(target_os = "ios")]
                bonjour,
            }),
        })
    }

    pub fn nickname(&self) -> String {
        self.runtime
            .shared
            .nickname
            .read()
            .expect("nickname lock")
            .clone()
    }

    #[cfg(target_os = "ios")]
    pub fn refresh_discovery(&self) -> Result<(), NetworkError> {
        self.runtime.bonjour.refresh_registration()
    }

    pub fn set_nickname(&self, value: &str) -> Result<(), NetworkError> {
        let nickname = normalize_nickname(value, &self.runtime.shared.identity_summary.display_id);
        let current = self.nickname();
        if nickname == current {
            return Ok(());
        }
        let avatar_hash = self.avatar_hash();

        #[cfg(target_os = "ios")]
        {
            self.runtime.bonjour.replace_registration(
                &current,
                avatar_hash.as_deref(),
                &nickname,
                avatar_hash.as_deref(),
            )?;
            *self.runtime.shared.nickname.write().expect("nickname lock") = nickname;
            Ok(())
        }

        #[cfg(not(target_os = "ios"))]
        {
            let replacement = discovery_service(
                &self.runtime.shared.identity_summary,
                &nickname,
                avatar_hash.as_deref(),
                &self.runtime.shared.certificate_fingerprint,
                self.runtime.shared.listening_port,
            )?;
            let replacement_fullname = replacement.get_fullname().to_owned();
            let current_fullname = self
                .runtime
                .service_fullname
                .read()
                .expect("service fullname lock")
                .clone();

            if let Ok(receiver) = self.runtime.mdns.unregister(&current_fullname) {
                let _ = receiver.recv_timeout(Duration::from_millis(250));
            }
            if let Err(error) = self.runtime.mdns.register(replacement) {
                if let Ok(previous) = discovery_service(
                    &self.runtime.shared.identity_summary,
                    &current,
                    avatar_hash.as_deref(),
                    &self.runtime.shared.certificate_fingerprint,
                    self.runtime.shared.listening_port,
                ) {
                    let _ = self.runtime.mdns.register(previous);
                }
                return Err(discovery_error(error));
            }

            *self.runtime.shared.nickname.write().expect("nickname lock") = nickname;
            *self
                .runtime
                .service_fullname
                .write()
                .expect("service fullname lock") = replacement_fullname;
            Ok(())
        }
    }

    pub fn avatar_hash(&self) -> Option<String> {
        self.runtime
            .shared
            .avatar
            .read()
            .expect("avatar lock")
            .as_ref()
            .map(|avatar| avatar.hash.clone())
    }

    pub fn set_avatar(&self, path: Option<&Path>) -> Result<(), NetworkError> {
        let replacement = path.map(local_avatar_from_path).transpose()?;
        let current = self
            .runtime
            .shared
            .avatar
            .read()
            .expect("avatar lock")
            .clone();
        if current.as_ref().map(|avatar| &avatar.hash)
            == replacement.as_ref().map(|avatar| &avatar.hash)
        {
            return Ok(());
        }
        let nickname = self.nickname();

        #[cfg(target_os = "ios")]
        {
            self.runtime.bonjour.replace_registration(
                &nickname,
                current.as_ref().map(|avatar| avatar.hash.as_str()),
                &nickname,
                replacement.as_ref().map(|avatar| avatar.hash.as_str()),
            )?;
            *self.runtime.shared.avatar.write().expect("avatar lock") = replacement;
            Ok(())
        }

        #[cfg(not(target_os = "ios"))]
        {
            let replacement_service = discovery_service(
                &self.runtime.shared.identity_summary,
                &nickname,
                replacement.as_ref().map(|avatar| avatar.hash.as_str()),
                &self.runtime.shared.certificate_fingerprint,
                self.runtime.shared.listening_port,
            )?;
            let replacement_fullname = replacement_service.get_fullname().to_owned();
            let current_fullname = self
                .runtime
                .service_fullname
                .read()
                .expect("service fullname lock")
                .clone();
            if let Ok(receiver) = self.runtime.mdns.unregister(&current_fullname) {
                let _ = receiver.recv_timeout(Duration::from_millis(250));
            }
            if let Err(error) = self.runtime.mdns.register(replacement_service) {
                if let Ok(previous) = discovery_service(
                    &self.runtime.shared.identity_summary,
                    &nickname,
                    current.as_ref().map(|avatar| avatar.hash.as_str()),
                    &self.runtime.shared.certificate_fingerprint,
                    self.runtime.shared.listening_port,
                ) {
                    let _ = self.runtime.mdns.register(previous);
                }
                return Err(discovery_error(error));
            }
            *self.runtime.shared.avatar.write().expect("avatar lock") = replacement;
            *self
                .runtime
                .service_fullname
                .write()
                .expect("service fullname lock") = replacement_fullname;
            Ok(())
        }
    }

    pub fn sync_peer_avatar(&self, peer_id: &str) -> Result<NetworkSnapshot, NetworkError> {
        let peer = {
            let state = self
                .runtime
                .shared
                .state
                .read()
                .expect("network state lock");
            state
                .peers
                .get(peer_id)
                .cloned()
                .ok_or(NetworkError::PeerOffline)?
        };
        if peer.trust_state != TrustState::Trusted {
            return Err(NetworkError::PeerUntrusted);
        }
        let endpoint = peer.endpoint.ok_or(NetworkError::PeerOffline)?;
        let (mut stream, authenticated) =
            connect_and_authenticate(&self.runtime.shared, endpoint, Some(&peer))?;
        let remote_hash = authenticated
            .avatar_hash
            .as_deref()
            .and_then(normalize_avatar_hash);
        upsert_authenticated_peer(&self.runtime.shared, authenticated, Some(endpoint))?;
        let cached_path = remote_hash
            .as_deref()
            .and_then(|hash| cached_avatar_path(&self.runtime.shared, peer_id, hash));
        let cached_hash = cached_path.as_ref().and(remote_hash.clone());
        write_frame(
            &mut stream,
            &WireFrame::AvatarRequest {
                cached_hash: cached_hash.clone(),
            },
        )?;

        let avatar_path = match read_frame(&mut stream)? {
            WireFrame::AvatarUnavailable => None,
            WireFrame::AvatarUnchanged { avatar_hash }
                if normalize_avatar_hash(&avatar_hash) == cached_hash =>
            {
                cached_path
            }
            WireFrame::AvatarOffer {
                avatar_hash,
                media_type,
                byte_size,
            } => {
                let avatar_hash = normalize_avatar_hash(&avatar_hash).ok_or_else(|| {
                    NetworkError::InvalidWire("peer avatar hash is invalid".to_owned())
                })?;
                if media_type != AVATAR_MEDIA_TYPE || byte_size == 0 || byte_size > MAX_AVATAR_BYTES
                {
                    return Err(NetworkError::InvalidWire(
                        "peer avatar metadata is invalid".to_owned(),
                    ));
                }
                let mut contents = vec![0_u8; byte_size as usize];
                stream.read_exact(&mut contents)?;
                if hex::encode(Sha256::digest(&contents)) != avatar_hash {
                    return Err(NetworkError::InvalidWire(
                        "peer avatar digest did not match".to_owned(),
                    ));
                }
                let path = peer_avatar_file(&self.runtime.shared, peer_id, &avatar_hash);
                let temporary = self
                    .runtime
                    .shared
                    .avatar_cache_dir
                    .join(format!("{peer_id}-{avatar_hash}.tmp"));
                fs::write(&temporary, contents)?;
                fs::rename(&temporary, &path)?;
                Some(path)
            }
            WireFrame::Rejected { reason } => return Err(NetworkError::PeerRejected(reason)),
            _ => {
                return Err(NetworkError::InvalidWire(
                    "expected avatar response".to_owned(),
                ));
            }
        };

        let mut state = self
            .runtime
            .shared
            .state
            .write()
            .expect("network state lock");
        let stored = state
            .peers
            .get_mut(peer_id)
            .ok_or(NetworkError::PeerOffline)?;
        stored.avatar_hash = remote_hash;
        stored.avatar_path = avatar_path;
        drop(state);
        Ok(self.snapshot())
    }

    pub fn snapshot(&self) -> NetworkSnapshot {
        let state = self
            .runtime
            .shared
            .state
            .read()
            .expect("network state lock");
        let mut peers = state
            .peers
            .values()
            .map(|peer| {
                let unread_count = state
                    .messages
                    .iter()
                    .filter(|message| {
                        message.peer_id == peer.peer_id
                            && message.direction == MessageDirection::Incoming
                            && !message.is_read
                    })
                    .count();
                peer.snapshot(
                    verification_code(&self.runtime.shared.identity_summary.peer_id, &peer.peer_id),
                    unread_count,
                )
            })
            .collect::<Vec<_>>();
        peers.sort_by(|left, right| {
            right
                .is_online
                .cmp(&left.is_online)
                .then_with(|| right.last_seen_unix_ms.cmp(&left.last_seen_unix_ms))
                .then_with(|| left.peer_id.cmp(&right.peer_id))
        });

        NetworkSnapshot {
            listening_port: self.runtime.shared.listening_port,
            local_endpoints: local_endpoints(self.runtime.shared.listening_port),
            active_network: self
                .runtime
                .shared
                .active_network
                .read()
                .expect("active network lock")
                .clone(),
            network_spaces: state.network_spaces.clone(),
            peers,
            messages: state.messages.clone(),
        }
    }

    pub fn set_active_network(
        &self,
        network: Option<ActiveNetwork>,
    ) -> Result<NetworkSnapshot, NetworkError> {
        if let Some(network) = &network {
            if network.network_id.trim().is_empty()
                || network.display_name.is_empty()
                || network.display_name.len() > 128
                || network.display_name.chars().any(char::is_control)
            {
                return Err(NetworkError::InvalidWire(
                    "current network identity is invalid".to_owned(),
                ));
            }
        }
        *self
            .runtime
            .shared
            .active_network
            .write()
            .expect("active network lock") = network;
        Ok(self.snapshot())
    }

    pub fn set_peer_trust(
        &self,
        peer_id: &str,
        trust_state: TrustState,
    ) -> Result<NetworkSnapshot, NetworkError> {
        let public_key = {
            let state = self
                .runtime
                .shared
                .state
                .read()
                .expect("network state lock");
            state
                .peers
                .get(peer_id)
                .map(|peer| peer.public_key.clone())
                .ok_or(NetworkError::PeerOffline)?
        };
        self.runtime
            .shared
            .store
            .set_trust_state(peer_id, &public_key, trust_state)?;
        let mut state = self
            .runtime
            .shared
            .state
            .write()
            .expect("network state lock");
        let peer = state
            .peers
            .get_mut(peer_id)
            .ok_or(NetworkError::PeerOffline)?;
        peer.trust_state = trust_state;
        drop(state);
        Ok(self.snapshot())
    }

    pub fn mark_peer_read(
        &self,
        peer_id: &str,
        network_id: &str,
    ) -> Result<NetworkSnapshot, NetworkError> {
        self.runtime
            .shared
            .store
            .mark_peer_read(peer_id, network_id)?;
        let mut state = self
            .runtime
            .shared
            .state
            .write()
            .expect("network state lock");
        for message in &mut state.messages {
            if message.peer_id == peer_id
                && message.network_id == network_id
                && message.direction == MessageDirection::Incoming
            {
                message.is_read = true;
            }
        }
        drop(state);
        Ok(self.snapshot())
    }

    pub fn load_older_messages(
        &self,
        network_id: &str,
        peer_id: &str,
        before_created_at_unix_ms: u64,
        before_message_id: &str,
        limit: usize,
    ) -> Result<HistoryPage, NetworkError> {
        if network_id.is_empty()
            || peer_id.is_empty()
            || before_message_id.is_empty()
            || !(1..=100).contains(&limit)
        {
            return Err(NetworkError::InvalidWire("历史消息请求无效".to_owned()));
        }
        let mut stored = self
            .runtime
            .shared
            .store
            .load_conversation_messages_before(
                network_id,
                peer_id,
                before_created_at_unix_ms,
                before_message_id,
                limit + 1,
            )?;
        let has_more = stored.len() > limit;
        if has_more {
            stored.remove(0);
        }
        let messages = stored
            .into_iter()
            .map(chat_message_from_stored)
            .collect::<Result<Vec<_>, _>>()?;
        let mut state = self
            .runtime
            .shared
            .state
            .write()
            .expect("network state lock");
        let known = state
            .messages
            .iter()
            .map(|message| message.message_id.clone())
            .collect::<HashSet<_>>();
        let mut loaded = 0;
        for message in messages {
            if !known.contains(&message.message_id) {
                state.messages.push(message);
                loaded += 1;
            }
        }
        state.messages.sort_by(|left, right| {
            left.created_at_unix_ms
                .cmp(&right.created_at_unix_ms)
                .then_with(|| left.message_id.cmp(&right.message_id))
        });
        drop(state);
        Ok(HistoryPage {
            snapshot: self.snapshot(),
            loaded,
            has_more,
        })
    }

    pub fn storage_summary(&self) -> Result<StorageSummary, NetworkError> {
        let messages = self
            .runtime
            .shared
            .store
            .load_all_messages()?
            .into_iter()
            .map(chat_message_from_stored)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(received_storage_summary(&messages))
    }

    pub fn clear_received_files(&self) -> Result<StorageSummary, NetworkError> {
        let mut messages = self
            .runtime
            .shared
            .store
            .load_all_messages()?
            .into_iter()
            .map(chat_message_from_stored)
            .collect::<Result<Vec<_>, _>>()?;
        let incoming_dir = self.runtime.shared.attachment_dir.join("incoming");
        let preview_dir = self.runtime.shared.attachment_dir.join("previews");
        let mut cleared = HashMap::new();

        for message in &mut messages {
            if message.direction != MessageDirection::Incoming {
                continue;
            }
            let ChatContent::Attachment { attachment } = &mut message.content else {
                continue;
            };
            if let Some(path) = attachment.local_path.take() {
                remove_managed_file(Path::new(&path), &incoming_dir)?;
            }
            if let Some(path) = attachment.preview_path.take() {
                remove_managed_file(Path::new(&path), &preview_dir)?;
            }
            persist_message(&self.runtime.shared, message)?;
            cleared.insert(message.message_id.clone(), message.clone());
        }

        let mut state = self
            .runtime
            .shared
            .state
            .write()
            .expect("network state lock");
        for message in &mut state.messages {
            if let Some(replacement) = cleared.get(&message.message_id) {
                *message = replacement.clone();
            }
        }
        drop(state);
        self.storage_summary()
    }

    pub fn connect_endpoint(&self, endpoint: &str) -> Result<NetworkSnapshot, NetworkError> {
        let endpoint = parse_manual_endpoint(endpoint)?;
        let (_stream, authenticated) =
            connect_and_authenticate(&self.runtime.shared, endpoint, None)?;
        if authenticated.peer_id == self.runtime.shared.identity_summary.peer_id {
            return Err(NetworkError::InvalidWire("不能连接本机地址".to_owned()));
        }
        upsert_manual_peer(&self.runtime.shared, endpoint, authenticated)?;
        Ok(self.snapshot())
    }

    pub fn send_text(&self, peer_id: &str, text: &str) -> Result<ChatMessage, NetworkError> {
        if text.trim().is_empty() {
            return Err(NetworkError::InvalidWire(
                "message text is empty".to_owned(),
            ));
        }
        if text.len() > MAX_TEXT_BYTES {
            return Err(NetworkError::InvalidWire(format!(
                "message is {} bytes; maximum is {MAX_TEXT_BYTES}",
                text.len()
            )));
        }

        let active_network = current_network(&self.runtime.shared)?;

        let peer = {
            let state = self
                .runtime
                .shared
                .state
                .read()
                .expect("network state lock");
            state
                .peers
                .get(peer_id)
                .cloned()
                .ok_or(NetworkError::PeerOffline)?
        };
        if peer.trust_state != TrustState::Trusted {
            return Err(NetworkError::PeerUntrusted);
        }
        let message_id = Uuid::new_v4().to_string();
        let conversation_id = conversation_id(
            &active_network.network_id,
            &self.runtime.shared.identity_summary.peer_id,
            &peer.peer_id,
        );
        let created_at_unix_ms = unix_time_ms();
        let message = ChatMessage {
            message_id,
            network_id: active_network.network_id.clone(),
            conversation_id,
            peer_id: peer.peer_id.clone(),
            direction: MessageDirection::Outgoing,
            delivery: DeliveryState::Sending,
            content: ChatContent::Text {
                text: text.to_owned(),
            },
            created_at_unix_ms,
            is_read: true,
        };
        persist_message(&self.runtime.shared, &message)?;
        self.runtime
            .shared
            .state
            .write()
            .expect("network state lock")
            .messages
            .push(message.clone());
        send_or_queue_outgoing(&self.runtime.shared, &active_network, &peer, message)
    }

    pub fn send_attachment(
        &self,
        peer_id: &str,
        source_path: &Path,
        kind: AttachmentKind,
        preferred_file_name: Option<&str>,
    ) -> Result<ChatMessage, NetworkError> {
        let active_network = current_network(&self.runtime.shared)?;
        let peer = {
            let state = self
                .runtime
                .shared
                .state
                .read()
                .expect("network state lock");
            state
                .peers
                .get(peer_id)
                .cloned()
                .ok_or(NetworkError::PeerOffline)?
        };
        if peer.trust_state != TrustState::Trusted {
            return Err(NetworkError::PeerUntrusted);
        }
        let prepared = prepare_outgoing_attachment(
            &self.runtime.shared,
            source_path,
            kind,
            preferred_file_name,
        )?;
        let message_id = Uuid::new_v4().to_string();
        let conversation_id = conversation_id(
            &active_network.network_id,
            &self.runtime.shared.identity_summary.peer_id,
            &peer.peer_id,
        );
        let created_at_unix_ms = unix_time_ms();
        let attachment = ChatAttachment {
            transfer_id: prepared.transfer_id.clone(),
            kind,
            file_name: prepared.file_name.clone(),
            media_type: prepared.media_type.clone(),
            byte_size: prepared.byte_size,
            transferred_bytes: 0,
            local_path: Some(path_string(&prepared.path)),
            preview_path: prepared.preview_path.as_deref().map(path_string),
        };
        let message = ChatMessage {
            message_id,
            network_id: active_network.network_id.clone(),
            conversation_id,
            peer_id: peer.peer_id.clone(),
            direction: MessageDirection::Outgoing,
            delivery: DeliveryState::Sending,
            content: ChatContent::Attachment { attachment },
            created_at_unix_ms,
            is_read: true,
        };
        persist_message(&self.runtime.shared, &message)?;
        self.runtime
            .shared
            .state
            .write()
            .expect("network state lock")
            .messages
            .push(message.clone());
        send_or_queue_outgoing(&self.runtime.shared, &active_network, &peer, message)
    }

    pub fn cancel_message(&self, message_id: &str) -> Result<NetworkSnapshot, NetworkError> {
        let can_cancel = {
            let state = self
                .runtime
                .shared
                .state
                .read()
                .expect("network state lock");
            state.messages.iter().any(|message| {
                message.message_id == message_id
                    && message.direction == MessageDirection::Outgoing
                    && message.delivery == DeliveryState::Sending
                    && matches!(message.content, ChatContent::Attachment { .. })
            })
        };
        if !can_cancel {
            return Err(NetworkError::InvalidWire("这条传输当前无法取消".to_owned()));
        }

        self.runtime
            .shared
            .cancelled_deliveries
            .lock()
            .expect("cancelled deliveries lock")
            .insert(message_id.to_owned());
        self.runtime
            .shared
            .delivery_retry_after
            .lock()
            .expect("delivery retry lock")
            .remove(message_id);
        update_outgoing_delivery(&self.runtime.shared, message_id, DeliveryState::Failed)?;
        let in_flight = self
            .runtime
            .shared
            .delivery_in_flight
            .lock()
            .expect("delivery in-flight lock")
            .contains(message_id);
        if !in_flight {
            clear_delivery_cancelled(&self.runtime.shared, message_id);
        }
        Ok(self.snapshot())
    }

    pub fn delete_message(&self, message_id: &str) -> Result<NetworkSnapshot, NetworkError> {
        let message = {
            let mut state = self
                .runtime
                .shared
                .state
                .write()
                .expect("network state lock");
            let index = state
                .messages
                .iter()
                .position(|message| message.message_id == message_id)
                .ok_or_else(|| NetworkError::InvalidWire("消息不存在或已删除".to_owned()))?;
            let message = state.messages.remove(index);
            if message.direction == MessageDirection::Outgoing
                && message.delivery == DeliveryState::Sending
            {
                self.runtime
                    .shared
                    .cancelled_deliveries
                    .lock()
                    .expect("cancelled deliveries lock")
                    .insert(message_id.to_owned());
            }
            message
        };

        if !self.runtime.shared.store.delete_message(message_id)? {
            self.runtime
                .shared
                .state
                .write()
                .expect("network state lock")
                .messages
                .push(message);
            return Err(NetworkError::InvalidWire("消息不存在或已删除".to_owned()));
        }
        self.runtime
            .shared
            .delivery_retry_after
            .lock()
            .expect("delivery retry lock")
            .remove(message_id);
        let in_flight = self
            .runtime
            .shared
            .delivery_in_flight
            .lock()
            .expect("delivery in-flight lock")
            .contains(message_id);
        if !in_flight {
            clear_delivery_cancelled(&self.runtime.shared, message_id);
        }
        remove_message_files(&self.runtime.shared, &message);
        Ok(self.snapshot())
    }

    pub fn retry_message(&self, message_id: &str) -> Result<ChatMessage, NetworkError> {
        let active_network = current_network(&self.runtime.shared)?;
        let (message, peer) = {
            let state = self
                .runtime
                .shared
                .state
                .read()
                .expect("network state lock");
            let message = state
                .messages
                .iter()
                .find(|message| message.message_id == message_id)
                .filter(|message| {
                    message.direction == MessageDirection::Outgoing
                        && message.delivery == DeliveryState::Failed
                })
                .cloned()
                .ok_or_else(|| NetworkError::InvalidWire("这条消息当前无法重试".to_owned()))?;
            if message.network_id != active_network.network_id {
                return Err(NetworkError::InvalidWire(
                    "请先切回发送这条消息时的 Wi-Fi".to_owned(),
                ));
            }
            let peer = state
                .peers
                .get(&message.peer_id)
                .cloned()
                .ok_or(NetworkError::PeerOffline)?;
            (message, peer)
        };
        if peer.trust_state != TrustState::Trusted {
            return Err(NetworkError::PeerUntrusted);
        }
        if let ChatContent::Attachment { attachment } = &message.content {
            let path = attachment
                .local_path
                .as_deref()
                .map(Path::new)
                .filter(|path| path.is_file())
                .ok_or_else(|| NetworkError::Attachment("待发送文件已被删除".to_owned()))?;
            if fs::metadata(path)?.len() != attachment.byte_size {
                return Err(NetworkError::Attachment("待发送文件已发生变化".to_owned()));
            }
        }

        clear_delivery_cancelled(&self.runtime.shared, message_id);
        self.runtime
            .shared
            .delivery_retry_after
            .lock()
            .expect("delivery retry lock")
            .remove(message_id);
        let message =
            update_outgoing_delivery(&self.runtime.shared, message_id, DeliveryState::Sending)?
                .ok_or_else(|| NetworkError::InvalidWire("待重试消息不存在".to_owned()))?;
        send_or_queue_outgoing(&self.runtime.shared, &active_network, &peer, message)
    }
}

fn load_persisted_state(store: &Store) -> Result<NodeState, NetworkError> {
    let peers = store
        .load_peers()?
        .into_iter()
        .map(|peer| {
            (
                peer.peer_id.clone(),
                PeerState {
                    peer_id: peer.peer_id,
                    display_id: peer.display_id,
                    alias: peer.alias,
                    public_key: peer.public_key,
                    certificate_fingerprint: peer.certificate_fingerprint,
                    endpoint: None,
                    service_fullnames: HashSet::new(),
                    last_seen_unix_ms: peer.last_seen_unix_ms,
                    trust_state: peer.trust_state,
                    avatar_hash: None,
                    avatar_path: None,
                },
            )
        })
        .collect();
    let network_spaces = store
        .load_network_spaces()?
        .into_iter()
        .map(network_space_from_stored)
        .collect();
    let mut messages = Vec::new();
    for stored in store.load_recent_messages(MAX_LOADED_MESSAGES)? {
        let mut message = chat_message_from_stored(stored)?;
        if message.delivery == DeliveryState::Receiving
            || (message.delivery == DeliveryState::Sending
                && message.direction != MessageDirection::Outgoing)
        {
            message.delivery = DeliveryState::Failed;
            store.save_message(&stored_message(&message)?)?;
        }
        messages.push(message);
    }
    Ok(NodeState {
        peers,
        service_peers: HashMap::new(),
        network_spaces,
        messages,
    })
}

fn stored_peer(peer: &PeerState) -> StoredPeer {
    StoredPeer {
        peer_id: peer.peer_id.clone(),
        display_id: peer.display_id.clone(),
        alias: peer.alias.clone(),
        public_key: peer.public_key.clone(),
        certificate_fingerprint: peer.certificate_fingerprint.clone(),
        last_endpoint: peer.endpoint.map(|endpoint| endpoint.to_string()),
        last_seen_unix_ms: peer.last_seen_unix_ms,
        trust_state: peer.trust_state,
    }
}

fn stored_message(message: &ChatMessage) -> Result<StoredMessage, NetworkError> {
    let content_json = serde_json::to_string(&message.content)
        .map_err(|error| NetworkError::InvalidWire(error.to_string()))?;
    Ok(StoredMessage {
        message_id: message.message_id.clone(),
        network_id: message.network_id.clone(),
        conversation_id: message.conversation_id.clone(),
        peer_id: message.peer_id.clone(),
        direction: match message.direction {
            MessageDirection::Incoming => "incoming",
            MessageDirection::Outgoing => "outgoing",
        }
        .to_owned(),
        delivery: match message.delivery {
            DeliveryState::Received => "received",
            DeliveryState::Receiving => "receiving",
            DeliveryState::Sending => "sending",
            DeliveryState::Delivered => "delivered",
            DeliveryState::Failed => "failed",
        }
        .to_owned(),
        content_json,
        created_at_unix_ms: message.created_at_unix_ms,
        is_read: message.is_read,
    })
}

fn chat_message_from_stored(message: StoredMessage) -> Result<ChatMessage, NetworkError> {
    let direction = match message.direction.as_str() {
        "incoming" => MessageDirection::Incoming,
        "outgoing" => MessageDirection::Outgoing,
        value => {
            return Err(NetworkError::Storage(StorageError::InvalidData(format!(
                "unknown message direction {value:?}"
            ))));
        }
    };
    let delivery = match message.delivery.as_str() {
        "received" => DeliveryState::Received,
        "receiving" => DeliveryState::Receiving,
        "sending" => DeliveryState::Sending,
        "delivered" => DeliveryState::Delivered,
        "failed" => DeliveryState::Failed,
        value => {
            return Err(NetworkError::Storage(StorageError::InvalidData(format!(
                "unknown delivery state {value:?}"
            ))));
        }
    };
    let content = serde_json::from_str(&message.content_json).map_err(|error| {
        NetworkError::Storage(StorageError::InvalidData(format!(
            "message content is invalid: {error}"
        )))
    })?;
    Ok(ChatMessage {
        message_id: message.message_id,
        network_id: message.network_id,
        conversation_id: message.conversation_id,
        peer_id: message.peer_id,
        direction,
        delivery,
        content,
        created_at_unix_ms: message.created_at_unix_ms,
        is_read: message.is_read,
    })
}

fn network_space_from_stored(network: StoredNetworkSpace) -> NetworkSpace {
    NetworkSpace {
        network_id: network.network_id,
        display_name: network.display_name,
        first_used_unix_ms: network.first_used_unix_ms,
        last_used_unix_ms: network.last_used_unix_ms,
    }
}

fn current_network(shared: &Shared) -> Result<ActiveNetwork, NetworkError> {
    shared
        .active_network
        .read()
        .expect("active network lock")
        .clone()
        .ok_or(NetworkError::NoActiveNetwork)
}

fn remember_network_usage(
    shared: &Shared,
    network: &ActiveNetwork,
    used_at_unix_ms: u64,
) -> Result<(), NetworkError> {
    let stored = StoredNetworkSpace {
        network_id: network.network_id.clone(),
        display_name: network.display_name.clone(),
        first_used_unix_ms: used_at_unix_ms,
        last_used_unix_ms: used_at_unix_ms,
    };
    shared.store.remember_network_space(&stored)?;
    let mut state = shared.state.write().expect("network state lock");
    match state
        .network_spaces
        .iter_mut()
        .find(|space| space.network_id == network.network_id)
    {
        Some(space) => {
            space.display_name = network.display_name.clone();
            space.first_used_unix_ms = space.first_used_unix_ms.min(used_at_unix_ms);
            space.last_used_unix_ms = space.last_used_unix_ms.max(used_at_unix_ms);
        }
        None => state.network_spaces.push(NetworkSpace {
            network_id: network.network_id.clone(),
            display_name: network.display_name.clone(),
            first_used_unix_ms: used_at_unix_ms,
            last_used_unix_ms: used_at_unix_ms,
        }),
    }
    state.network_spaces.sort_by(|left, right| {
        right
            .last_used_unix_ms
            .cmp(&left.last_used_unix_ms)
            .then_with(|| left.network_id.cmp(&right.network_id))
    });
    Ok(())
}

fn persist_message(shared: &Shared, message: &ChatMessage) -> Result<(), NetworkError> {
    shared.store.save_message(&stored_message(message)?)?;
    Ok(())
}

fn send_or_queue_outgoing(
    shared: &Arc<Shared>,
    active_network: &ActiveNetwork,
    peer: &PeerState,
    message: ChatMessage,
) -> Result<ChatMessage, NetworkError> {
    let Some(endpoint) = peer.endpoint else {
        remember_network_usage(shared, active_network, message.created_at_unix_ms)?;
        return Ok(message);
    };
    if !begin_delivery(shared, &message.message_id) {
        return Ok(message);
    }
    let result = attempt_outgoing_delivery(shared, peer, endpoint, &message);
    finish_delivery(shared, &message.message_id);
    let should_remember_network =
        result.is_ok() || result.as_ref().is_err_and(is_retryable_delivery_error);
    let created_at_unix_ms = message.created_at_unix_ms;
    let completed = complete_outgoing_attempt(shared, message, result);
    if should_remember_network {
        remember_network_usage(shared, active_network, created_at_unix_ms)?;
    }
    completed
}

fn begin_delivery(shared: &Shared, message_id: &str) -> bool {
    if delivery_is_cancelled(shared, message_id) {
        return false;
    }
    shared
        .delivery_in_flight
        .lock()
        .expect("delivery in-flight lock")
        .insert(message_id.to_owned())
}

fn finish_delivery(shared: &Shared, message_id: &str) {
    shared
        .delivery_in_flight
        .lock()
        .expect("delivery in-flight lock")
        .remove(message_id);
}

fn complete_outgoing_attempt(
    shared: &Shared,
    message: ChatMessage,
    result: Result<(), NetworkError>,
) -> Result<ChatMessage, NetworkError> {
    if take_delivery_cancelled(shared, &message.message_id) {
        return Ok(
            update_outgoing_delivery(shared, &message.message_id, DeliveryState::Failed)?
                .unwrap_or(message),
        );
    }
    match result {
        Ok(()) => {
            shared
                .delivery_retry_after
                .lock()
                .expect("delivery retry lock")
                .remove(&message.message_id);
            update_outgoing_delivery(shared, &message.message_id, DeliveryState::Delivered)?
                .ok_or_else(|| NetworkError::InvalidWire("outgoing message disappeared".to_owned()))
        }
        Err(error) if is_retryable_delivery_error(&error) => {
            shared
                .delivery_retry_after
                .lock()
                .expect("delivery retry lock")
                .insert(
                    message.message_id.clone(),
                    unix_time_ms().saturating_add(DELIVERY_RETRY_DELAY_MS),
                );
            update_outgoing_delivery(shared, &message.message_id, DeliveryState::Sending)?
                .ok_or_else(|| NetworkError::InvalidWire("outgoing message disappeared".to_owned()))
        }
        Err(error) => {
            update_outgoing_delivery(shared, &message.message_id, DeliveryState::Failed)?;
            Err(error)
        }
    }
}

fn update_outgoing_delivery(
    shared: &Shared,
    message_id: &str,
    delivery: DeliveryState,
) -> Result<Option<ChatMessage>, NetworkError> {
    let mut state = shared.state.write().expect("network state lock");
    let updated = state
        .messages
        .iter_mut()
        .find(|stored| stored.message_id == message_id)
        .map(|stored| {
            stored.delivery = delivery;
            if delivery == DeliveryState::Sending {
                if let ChatContent::Attachment { attachment } = &mut stored.content {
                    attachment.transferred_bytes = 0;
                }
            }
            stored.clone()
        });
    drop(state);
    if let Some(updated) = &updated {
        persist_message(shared, updated)?;
    }
    Ok(updated)
}

fn is_retryable_delivery_error(error: &NetworkError) -> bool {
    matches!(
        error,
        NetworkError::Io(_)
            | NetworkError::Tls(_)
            | NetworkError::PeerOffline
            | NetworkError::InvalidAcknowledgement
    )
}

fn delivery_is_cancelled(shared: &Shared, message_id: &str) -> bool {
    shared
        .cancelled_deliveries
        .lock()
        .expect("cancelled deliveries lock")
        .contains(message_id)
}

fn clear_delivery_cancelled(shared: &Shared, message_id: &str) {
    shared
        .cancelled_deliveries
        .lock()
        .expect("cancelled deliveries lock")
        .remove(message_id);
}

fn take_delivery_cancelled(shared: &Shared, message_id: &str) -> bool {
    shared
        .cancelled_deliveries
        .lock()
        .expect("cancelled deliveries lock")
        .remove(message_id)
}

fn attempt_outgoing_delivery(
    shared: &Shared,
    peer: &PeerState,
    endpoint: SocketAddr,
    message: &ChatMessage,
) -> Result<(), NetworkError> {
    let sequence = shared.sequence.fetch_add(1, Ordering::Relaxed);
    let payload = match &message.content {
        ChatContent::Text { text } => Payload::Text { text: text.clone() },
        ChatContent::Attachment { attachment } => Payload::Attachment {
            transfer_id: attachment.transfer_id.clone(),
            kind: attachment.kind,
            file_name: attachment.file_name.clone(),
            media_type: attachment.media_type.clone(),
            byte_size: attachment.byte_size,
        },
    };
    let envelope = Envelope {
        protocol_version: PROTOCOL_VERSION,
        message_id: message.message_id.clone(),
        network_id: message.network_id.clone(),
        conversation_id: message.conversation_id.clone(),
        sender_peer_id: shared.identity_summary.peer_id.clone(),
        sender_sequence: sequence,
        created_at_unix_ms: message.created_at_unix_ms,
        payload,
    };
    match &message.content {
        ChatContent::Text { .. } => send_envelope(shared, peer, endpoint, envelope),
        ChatContent::Attachment { attachment } => {
            let path = attachment
                .local_path
                .as_deref()
                .map(PathBuf::from)
                .ok_or_else(|| NetworkError::Attachment("待发送文件不存在".to_owned()))?;
            let prepared = PreparedAttachment {
                transfer_id: attachment.transfer_id.clone(),
                path,
                preview_path: attachment.preview_path.as_deref().map(PathBuf::from),
                file_name: attachment.file_name.clone(),
                media_type: attachment.media_type.clone(),
                byte_size: attachment.byte_size,
            };
            send_attachment_stream(
                shared,
                peer,
                endpoint,
                envelope,
                &prepared,
                &message.message_id,
            )
        }
    }
}

fn upsert_authenticated_peer(
    shared: &Shared,
    authenticated: AuthenticatedPeer,
    endpoint: Option<SocketAddr>,
) -> Result<TrustState, NetworkError> {
    let AuthenticatedPeer {
        peer_id,
        display_id,
        nickname,
        avatar_hash,
        public_key,
        certificate_fingerprint,
    } = authenticated;
    let avatar_hash = avatar_hash.and_then(|hash| normalize_avatar_hash(&hash));
    let avatar_path = avatar_hash
        .as_deref()
        .and_then(|hash| cached_avatar_path(shared, &peer_id, hash));
    let mut authenticated = PeerState {
        alias: normalize_nickname(nickname.as_deref().unwrap_or_default(), &display_id),
        peer_id: peer_id.clone(),
        display_id,
        public_key: public_key.clone(),
        certificate_fingerprint,
        endpoint,
        service_fullnames: HashSet::new(),
        last_seen_unix_ms: unix_time_ms(),
        trust_state: TrustState::Discovered,
        avatar_hash,
        avatar_path,
    };
    authenticated.trust_state = shared.store.remember_peer(&stored_peer(&authenticated))?;
    let mut state = shared.state.write().expect("network state lock");
    match state.peers.get_mut(&peer_id) {
        Some(existing) => {
            if existing.public_key != public_key {
                return Err(NetworkError::PeerIdentityMismatch);
            }
            existing.display_id = authenticated.display_id;
            existing.alias = authenticated.alias;
            existing.certificate_fingerprint = authenticated.certificate_fingerprint;
            if authenticated.endpoint.is_some() {
                existing.endpoint = authenticated.endpoint;
            }
            existing.last_seen_unix_ms = authenticated.last_seen_unix_ms;
            existing.trust_state = authenticated.trust_state;
            existing.avatar_hash = authenticated.avatar_hash;
            existing.avatar_path = authenticated.avatar_path;
        }
        None => {
            state.peers.insert(peer_id, authenticated.clone());
        }
    }
    Ok(authenticated.trust_state)
}

#[derive(Clone)]
struct AuthenticatedPeer {
    peer_id: String,
    display_id: String,
    nickname: Option<String>,
    avatar_hash: Option<String>,
    public_key: String,
    certificate_fingerprint: String,
}

fn upsert_manual_peer(
    shared: &Shared,
    endpoint: SocketAddr,
    authenticated: AuthenticatedPeer,
) -> Result<(), NetworkError> {
    upsert_authenticated_peer(shared, authenticated, Some(endpoint))?;
    Ok(())
}

impl Drop for NodeRuntime {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Relaxed);
        #[cfg(not(target_os = "ios"))]
        {
            let _ = self.mdns.stop_browse(SERVICE_TYPE);
            let service_fullname = self
                .service_fullname
                .read()
                .expect("service fullname lock")
                .clone();
            if let Ok(receiver) = self.mdns.unregister(&service_fullname) {
                let _ = receiver.recv_timeout(Duration::from_millis(250));
            }
            if let Ok(receiver) = self.mdns.shutdown() {
                let _ = receiver.recv_timeout(Duration::from_millis(250));
            }
        }
    }
}

fn spawn_listener(shared: Arc<Shared>, listener: TcpListener) {
    thread::Builder::new()
        .name("tossit-lan-listener".to_owned())
        .spawn(move || {
            while !shared.shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, remote)) => {
                        let shared = Arc::clone(&shared);
                        let _ = thread::Builder::new()
                            .name("tossit-lan-connection".to_owned())
                            .spawn(move || {
                                if let Err(error) = handle_incoming(&shared, stream) {
                                    eprintln!(
                                        "TossIt rejected local connection from {remote}: {error}"
                                    );
                                }
                            });
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(ACCEPT_POLL_INTERVAL);
                    }
                    Err(_) => thread::sleep(ACCEPT_POLL_INTERVAL),
                }
            }
        })
        .expect("spawn local network listener");
}

fn spawn_pending_delivery_worker(shared: Arc<Shared>) {
    thread::Builder::new()
        .name("tossit-pending-delivery".to_owned())
        .spawn(move || {
            while !shared.shutdown.load(Ordering::Relaxed) {
                retry_pending_deliveries_once(&shared);
                thread::sleep(PENDING_DELIVERY_POLL_INTERVAL);
            }
        })
        .expect("spawn pending delivery worker");
}

fn retry_pending_deliveries_once(shared: &Arc<Shared>) {
    let Some(active_network_id) = shared
        .active_network
        .read()
        .expect("active network lock")
        .as_ref()
        .map(|network| network.network_id.clone())
    else {
        return;
    };
    let now = unix_time_ms();
    let retry_after = shared
        .delivery_retry_after
        .lock()
        .expect("delivery retry lock")
        .clone();
    let pending = {
        let state = shared.state.read().expect("network state lock");
        state
            .messages
            .iter()
            .filter(|message| {
                message.direction == MessageDirection::Outgoing
                    && message.delivery == DeliveryState::Sending
                    && message.network_id == active_network_id
                    && retry_after.get(&message.message_id).copied().unwrap_or(0) <= now
            })
            .filter_map(|message| {
                let peer = state.peers.get(&message.peer_id)?.clone();
                let endpoint = peer.endpoint?;
                (peer.trust_state == TrustState::Trusted).then(|| (message.clone(), peer, endpoint))
            })
            .collect::<Vec<_>>()
    };

    for (message, peer, endpoint) in pending {
        if !begin_delivery(shared, &message.message_id) {
            continue;
        }
        let result = attempt_outgoing_delivery(shared, &peer, endpoint, &message);
        finish_delivery(shared, &message.message_id);
        if let Err(error) = complete_outgoing_attempt(shared, message, result) {
            eprintln!("TossIt could not deliver queued message: {error}");
        }
    }
}

#[cfg(not(target_os = "ios"))]
fn spawn_browser(shared: Arc<Shared>, receiver: mdns_sd::Receiver<ServiceEvent>) {
    thread::Builder::new()
        .name("tossit-mdns-browser".to_owned())
        .spawn(move || {
            while let Ok(event) = receiver.recv() {
                if shared.shutdown.load(Ordering::Relaxed) {
                    break;
                }
                match event {
                    ServiceEvent::ServiceResolved(service) => {
                        if let Some(peer) = peer_from_service(&service, &shared) {
                            upsert_discovered_peer(&shared, service.get_fullname(), peer);
                        }
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        remove_discovered_service(&shared, &fullname);
                    }
                    _ => {}
                }
            }
        })
        .expect("spawn local discovery browser");
}

#[cfg(not(target_os = "ios"))]
fn peer_from_service(service: &mdns_sd::ResolvedService, shared: &Shared) -> Option<PeerState> {
    let peer_id = service.get_property_val_str("peer")?.to_owned();
    if peer_id == shared.identity_summary.peer_id {
        return None;
    }
    let display_id = service.get_property_val_str("display")?.to_owned();
    let nickname = service.get_property_val_str("nickname").unwrap_or_default();
    let avatar_hash = service
        .get_property_val_str("avatar")
        .and_then(normalize_avatar_hash);
    let public_key = service.get_property_val_str("key")?.to_owned();
    let certificate_fingerprint = service.get_property_val_str("cert")?.to_owned();
    let version = service.get_property_val_str("v")?;
    if version != PROTOCOL_VERSION.to_string()
        || DeviceIdentity::peer_id_for_public_key(&public_key).ok()? != peer_id
        || display_id_for_peer(&peer_id)? != display_id
        || certificate_fingerprint.len() != 64
    {
        return None;
    }

    let mut addresses = service.get_addresses_v4();
    if let Ok(resolved) = (service.get_hostname(), service.get_port()).to_socket_addrs() {
        addresses.extend(resolved.filter_map(|endpoint| match endpoint.ip() {
            IpAddr::V4(address) => Some(address),
            IpAddr::V6(_) => None,
        }));
    }
    let endpoint = preferred_ipv4(addresses)
        .map(|address| SocketAddr::new(IpAddr::V4(address), service.get_port()))?;
    let mut service_fullnames = HashSet::new();
    service_fullnames.insert(service.get_fullname().to_owned());
    let avatar_path = avatar_hash
        .as_deref()
        .and_then(|hash| cached_avatar_path(shared, &peer_id, hash));

    Some(PeerState {
        alias: normalize_nickname(nickname, &display_id),
        peer_id,
        display_id,
        public_key,
        certificate_fingerprint,
        endpoint: Some(endpoint),
        service_fullnames,
        last_seen_unix_ms: unix_time_ms(),
        trust_state: TrustState::Discovered,
        avatar_path,
        avatar_hash,
    })
}

#[cfg(not(target_os = "ios"))]
fn preferred_ipv4(addresses: HashSet<Ipv4Addr>) -> Option<Ipv4Addr> {
    let mut addresses = addresses.into_iter().collect::<Vec<_>>();
    addresses.sort_by_key(|address| {
        (
            address.is_unspecified() || address.is_broadcast(),
            address.is_loopback(),
            address.is_link_local(),
            address.octets(),
        )
    });
    addresses
        .into_iter()
        .find(|address| !address.is_unspecified() && !address.is_broadcast())
}

fn local_endpoints(listening_port: u16) -> Vec<String> {
    let mut endpoints = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|interface| match interface.ip() {
            IpAddr::V4(address)
                if !address.is_loopback()
                    && !address.is_unspecified()
                    && !address.is_broadcast()
                    && !address.is_multicast() =>
            {
                Some(SocketAddr::new(IpAddr::V4(address), listening_port).to_string())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    endpoints.sort();
    endpoints.dedup();
    endpoints
}

fn parse_manual_endpoint(value: &str) -> Result<SocketAddr, NetworkError> {
    let value = value
        .trim()
        .strip_prefix("http://")
        .or_else(|| value.trim().strip_prefix("https://"))
        .unwrap_or(value.trim())
        .trim_end_matches('/');
    let endpoint = SocketAddr::from_str(value).map_err(|_| {
        NetworkError::InvalidWire(
            "地址格式不正确，请输入 IP:端口，例如 192.168.1.8:42318".to_owned(),
        )
    })?;
    let invalid_ip = match endpoint.ip() {
        IpAddr::V4(address) => {
            address.is_unspecified() || address.is_broadcast() || address.is_multicast()
        }
        IpAddr::V6(address) => address.is_unspecified() || address.is_multicast(),
    };
    if endpoint.port() == 0 || invalid_ip {
        return Err(NetworkError::InvalidWire(
            "该 IP 或端口不能用于连接设备".to_owned(),
        ));
    }
    Ok(endpoint)
}

fn upsert_discovered_peer(shared: &Shared, fullname: &str, mut peer: PeerState) {
    match shared.store.remember_peer(&stored_peer(&peer)) {
        Ok(trust_state) => peer.trust_state = trust_state,
        Err(error) => {
            eprintln!("TossIt could not persist discovered device: {error}");
            return;
        }
    }
    let mut state = shared.state.write().expect("network state lock");
    state
        .service_peers
        .insert(fullname.to_owned(), peer.peer_id.clone());
    let peer_id = peer.peer_id.clone();
    match state.peers.get_mut(&peer.peer_id) {
        Some(existing) => {
            existing.display_id = peer.display_id;
            existing.alias = peer.alias;
            existing.public_key = peer.public_key;
            existing.certificate_fingerprint = peer.certificate_fingerprint;
            if prefer_discovered_endpoint(existing.endpoint, peer.endpoint) {
                existing.endpoint = peer.endpoint;
            }
            existing.last_seen_unix_ms = peer.last_seen_unix_ms;
            existing.trust_state = peer.trust_state;
            if existing.avatar_hash != peer.avatar_hash {
                existing.avatar_hash = peer.avatar_hash;
                existing.avatar_path = peer.avatar_path;
            } else if peer.avatar_path.is_some() {
                existing.avatar_path = peer.avatar_path;
            }
            existing.service_fullnames.insert(fullname.to_owned());
        }
        None => {
            state.peers.insert(peer.peer_id.clone(), peer);
        }
    }
    let peer_message_ids = state
        .messages
        .iter()
        .filter(|message| message.peer_id == peer_id)
        .map(|message| message.message_id.clone())
        .collect::<HashSet<_>>();
    drop(state);
    shared
        .delivery_retry_after
        .lock()
        .expect("delivery retry lock")
        .retain(|message_id, _| !peer_message_ids.contains(message_id));
}

fn prefer_discovered_endpoint(current: Option<SocketAddr>, candidate: Option<SocketAddr>) -> bool {
    match (current, candidate) {
        (_, None) => false,
        (None, Some(_)) => true,
        (Some(current), Some(candidate)) => {
            endpoint_preference(candidate) <= endpoint_preference(current)
        }
    }
}

fn endpoint_preference(endpoint: SocketAddr) -> (bool, bool, bool) {
    let address = endpoint.ip();
    let is_link_local = match address {
        IpAddr::V4(address) => address.is_link_local(),
        IpAddr::V6(address) => address.is_unicast_link_local(),
    };
    (
        address.is_unspecified() || address.is_multicast(),
        address.is_loopback(),
        is_link_local,
    )
}

fn remove_discovered_service(shared: &Shared, fullname: &str) {
    let mut state = shared.state.write().expect("network state lock");
    let Some(peer_id) = state.service_peers.remove(fullname) else {
        return;
    };
    if let Some(peer) = state.peers.get_mut(&peer_id) {
        peer.service_fullnames.remove(fullname);
        if peer.service_fullnames.is_empty() {
            peer.endpoint = None;
        }
    }
}

fn handle_incoming(shared: &Shared, stream: TcpStream) -> Result<(), NetworkError> {
    let remote_ip = stream.peer_addr()?.ip();
    configure_stream(&stream)?;
    let connection = ServerConnection::new(Arc::clone(&shared.server_config)).map_err(tls_error)?;
    let mut stream = StreamOwned::new(connection, stream);
    let server_nonce = random_nonce()?;
    write_frame(
        &mut stream,
        &WireFrame::Challenge {
            protocol_version: PROTOCOL_VERSION,
            server_nonce: server_nonce.clone(),
            certificate_fingerprint: shared.certificate_fingerprint.clone(),
        },
    )?;

    let WireFrame::IdentityProof {
        peer_id,
        display_id,
        nickname,
        avatar_hash,
        public_key,
        certificate_fingerprint: client_certificate_fingerprint,
        listening_port,
        client_nonce,
        signature,
    } = read_frame(&mut stream)?
    else {
        return Err(NetworkError::InvalidWire(
            "expected device identity proof".to_owned(),
        ));
    };
    verify_peer_identity(&peer_id, &display_id, &public_key)?;
    validate_certificate_fingerprint(&client_certificate_fingerprint)?;
    if listening_port == 0 {
        return Err(NetworkError::InvalidWire(
            "peer listening port is invalid".to_owned(),
        ));
    }
    let proof = client_proof_bytes(
        &server_nonce,
        &client_nonce,
        &shared.certificate_fingerprint,
        &peer_id,
        &client_certificate_fingerprint,
        listening_port,
    );
    DeviceIdentity::verify(&public_key, &proof, &signature)?;

    let trust_state = upsert_authenticated_peer(
        shared,
        AuthenticatedPeer {
            peer_id: peer_id.clone(),
            display_id: display_id.clone(),
            nickname,
            avatar_hash,
            public_key: public_key.clone(),
            certificate_fingerprint: client_certificate_fingerprint,
        },
        Some(SocketAddr::new(remote_ip, listening_port)),
    )?;

    let accepted_proof = server_proof_bytes(
        &server_nonce,
        &client_nonce,
        &shared.certificate_fingerprint,
        &peer_id,
        &shared.identity_summary.peer_id,
    );
    write_frame(
        &mut stream,
        &WireFrame::IdentityAccepted {
            peer_id: shared.identity_summary.peer_id.clone(),
            display_id: shared.identity_summary.display_id.clone(),
            nickname: Some(shared.nickname.read().expect("nickname lock").clone()),
            avatar_hash: shared
                .avatar
                .read()
                .expect("avatar lock")
                .as_ref()
                .map(|avatar| avatar.hash.clone()),
            public_key: shared.identity_summary.public_key.clone(),
            signature: shared.identity.sign(&accepted_proof),
        },
    )?;
    if trust_state != TrustState::Trusted {
        write_frame(
            &mut stream,
            &WireFrame::Rejected {
                reason: "请先在这台设备上确认校验码".to_owned(),
            },
        )?;
        return Err(NetworkError::PeerUntrusted);
    }
    match read_frame(&mut stream)? {
        WireFrame::Message { envelope } => receive_text(shared, &peer_id, envelope, &mut stream),
        WireFrame::TransferOffer { envelope } => {
            let message_id = envelope.message_id.clone();
            let result = receive_attachment(shared, &peer_id, envelope, &mut stream);
            if result.is_err() {
                mark_delivery(shared, &message_id, DeliveryState::Failed);
            }
            result
        }
        WireFrame::AvatarRequest { cached_hash } => {
            send_avatar(shared, cached_hash.as_deref(), &mut stream)
        }
        _ => Err(NetworkError::InvalidWire(
            "expected message, attachment, or avatar request".to_owned(),
        )),
    }
}

fn send_avatar(
    shared: &Shared,
    cached_hash: Option<&str>,
    stream: &mut StreamOwned<ServerConnection, TcpStream>,
) -> Result<(), NetworkError> {
    let avatar = shared.avatar.read().expect("avatar lock").clone();
    let Some(avatar) = avatar else {
        return write_frame(stream, &WireFrame::AvatarUnavailable);
    };
    if cached_hash.and_then(normalize_avatar_hash).as_deref() == Some(avatar.hash.as_str()) {
        return write_frame(
            stream,
            &WireFrame::AvatarUnchanged {
                avatar_hash: avatar.hash,
            },
        );
    }
    let byte_size = avatar.contents.len() as u64;
    if byte_size == 0 || byte_size > MAX_AVATAR_BYTES {
        return Err(NetworkError::InvalidWire(
            "local avatar file is invalid".to_owned(),
        ));
    }
    write_frame(
        stream,
        &WireFrame::AvatarOffer {
            avatar_hash: avatar.hash,
            media_type: AVATAR_MEDIA_TYPE.to_owned(),
            byte_size,
        },
    )?;
    stream.write_all(&avatar.contents)?;
    stream.flush()?;
    Ok(())
}

fn receive_text(
    shared: &Shared,
    peer_id: &str,
    envelope: Envelope,
    stream: &mut StreamOwned<ServerConnection, TcpStream>,
) -> Result<(), NetworkError> {
    validate_peer_envelope(shared, peer_id, &envelope)?;
    let Payload::Text { text } = &envelope.payload else {
        return Err(NetworkError::InvalidWire(
            "expected text message".to_owned(),
        ));
    };
    insert_message_if_new(
        shared,
        ChatMessage {
            message_id: envelope.message_id.clone(),
            network_id: envelope.network_id.clone(),
            conversation_id: envelope.conversation_id.clone(),
            peer_id: peer_id.to_owned(),
            direction: MessageDirection::Incoming,
            delivery: DeliveryState::Received,
            content: ChatContent::Text { text: text.clone() },
            created_at_unix_ms: envelope.created_at_unix_ms,
            is_read: false,
        },
    )?;
    remember_network_usage(
        shared,
        &current_network(shared)?,
        envelope.created_at_unix_ms,
    )?;
    write_acknowledgement(shared, stream, envelope)
}

fn receive_attachment(
    shared: &Shared,
    peer_id: &str,
    envelope: Envelope,
    stream: &mut StreamOwned<ServerConnection, TcpStream>,
) -> Result<(), NetworkError> {
    validate_peer_envelope(shared, peer_id, &envelope)?;
    let Payload::Attachment {
        transfer_id,
        kind,
        file_name,
        media_type,
        byte_size,
    } = &envelope.payload
    else {
        return Err(NetworkError::InvalidWire(
            "expected attachment metadata".to_owned(),
        ));
    };
    let incoming_dir = shared.attachment_dir.join("incoming");
    if let Err(error) = ensure_free_space(&incoming_dir, *byte_size) {
        write_frame(
            stream,
            &WireFrame::Rejected {
                reason: "对方设备存储空间不足".to_owned(),
            },
        )?;
        return Err(error);
    }
    insert_message_if_new(
        shared,
        ChatMessage {
            message_id: envelope.message_id.clone(),
            network_id: envelope.network_id.clone(),
            conversation_id: envelope.conversation_id.clone(),
            peer_id: peer_id.to_owned(),
            direction: MessageDirection::Incoming,
            delivery: DeliveryState::Receiving,
            content: ChatContent::Attachment {
                attachment: ChatAttachment {
                    transfer_id: transfer_id.clone(),
                    kind: *kind,
                    file_name: file_name.clone(),
                    media_type: media_type.clone(),
                    byte_size: *byte_size,
                    transferred_bytes: 0,
                    local_path: None,
                    preview_path: None,
                },
            },
            created_at_unix_ms: envelope.created_at_unix_ms,
            is_read: false,
        },
    )?;
    write_frame(
        stream,
        &WireFrame::TransferReady {
            transfer_id: transfer_id.clone(),
        },
    )?;
    stream.sock.set_read_timeout(Some(TRANSFER_TIMEOUT))?;
    stream.sock.set_write_timeout(Some(TRANSFER_TIMEOUT))?;

    let mut temporary = tempfile::NamedTempFile::new_in(&incoming_dir)?;
    let mut hasher = Sha256::new();
    let mut remaining = *byte_size;
    let mut transferred = 0_u64;
    let mut buffer = vec![0_u8; TRANSFER_CHUNK_BYTES];
    while remaining > 0 {
        let requested = usize::try_from(remaining.min(TRANSFER_CHUNK_BYTES as u64))
            .expect("chunk size fits usize");
        stream.read_exact(&mut buffer[..requested])?;
        temporary.write_all(&buffer[..requested])?;
        hasher.update(&buffer[..requested]);
        remaining -= requested as u64;
        transferred += requested as u64;
        update_transferred_bytes(shared, &envelope.message_id, transferred);
    }
    temporary.as_file_mut().sync_all()?;
    let WireFrame::TransferDigest {
        transfer_id: received_transfer_id,
        sha256,
    } = read_frame(stream)?
    else {
        return Err(NetworkError::InvalidWire(
            "expected attachment digest".to_owned(),
        ));
    };
    let computed = hex::encode_upper(hasher.finalize());
    if received_transfer_id != *transfer_id
        || sha256.len() != 64
        || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !computed.eq_ignore_ascii_case(&sha256)
    {
        return Err(NetworkError::Attachment(
            "attachment checksum did not match".to_owned(),
        ));
    }

    let destination = incoming_dir.join(format!("{}-{file_name}", Uuid::new_v4()));
    temporary
        .persist(&destination)
        .map_err(|error| NetworkError::Io(error.error))?;
    set_owner_only(&destination)?;
    let preview_path = match kind {
        AttachmentKind::Image => Some(generate_thumbnail(shared, &destination)?),
        AttachmentKind::File => None,
    };
    complete_received_attachment(
        shared,
        &envelope.message_id,
        &destination,
        preview_path.as_deref(),
        *byte_size,
    )?;
    remember_network_usage(
        shared,
        &current_network(shared)?,
        envelope.created_at_unix_ms,
    )?;
    write_acknowledgement(shared, stream, envelope)
}

fn validate_peer_envelope(
    shared: &Shared,
    peer_id: &str,
    envelope: &Envelope,
) -> Result<(), NetworkError> {
    envelope
        .validate()
        .map_err(|error| NetworkError::InvalidWire(error.to_string()))?;
    let active_network = current_network(shared)?;
    if envelope.sender_peer_id != peer_id
        || envelope.network_id != active_network.network_id
        || envelope.conversation_id
            != conversation_id(
                &active_network.network_id,
                &shared.identity_summary.peer_id,
                peer_id,
            )
    {
        return Err(NetworkError::PeerIdentityMismatch);
    }
    Ok(())
}

fn insert_message_if_new(shared: &Shared, incoming: ChatMessage) -> Result<(), NetworkError> {
    let already_present = shared
        .state
        .read()
        .expect("network state lock")
        .messages
        .iter()
        .any(|message| message.message_id == incoming.message_id);
    if already_present {
        return Ok(());
    }
    persist_message(shared, &incoming)?;
    let mut state = shared.state.write().expect("network state lock");
    if !state
        .messages
        .iter()
        .any(|message| message.message_id == incoming.message_id)
    {
        state.messages.push(incoming);
    }
    Ok(())
}

fn write_acknowledgement(
    shared: &Shared,
    stream: &mut impl Write,
    envelope: Envelope,
) -> Result<(), NetworkError> {
    let acknowledgement = Envelope {
        protocol_version: PROTOCOL_VERSION,
        message_id: Uuid::new_v4().to_string(),
        network_id: envelope.network_id,
        conversation_id: envelope.conversation_id,
        sender_peer_id: shared.identity_summary.peer_id.clone(),
        sender_sequence: shared.sequence.fetch_add(1, Ordering::Relaxed),
        created_at_unix_ms: unix_time_ms(),
        payload: Payload::Acknowledgement {
            acknowledged_message_id: envelope.message_id,
        },
    };
    write_frame(
        stream,
        &WireFrame::Message {
            envelope: acknowledgement,
        },
    )
}

fn send_envelope(
    shared: &Shared,
    peer: &PeerState,
    endpoint: SocketAddr,
    envelope: Envelope,
) -> Result<(), NetworkError> {
    let mut stream = connect_authenticated(shared, peer, endpoint)?;
    let sent_message_id = envelope.message_id.clone();
    write_frame(&mut stream, &WireFrame::Message { envelope })?;
    read_matching_acknowledgement(&mut stream, peer, &sent_message_id)
}

fn connect_authenticated(
    shared: &Shared,
    peer: &PeerState,
    endpoint: SocketAddr,
) -> Result<StreamOwned<ClientConnection, TcpStream>, NetworkError> {
    let (stream, authenticated) = connect_and_authenticate(shared, endpoint, Some(peer))?;
    upsert_authenticated_peer(shared, authenticated, Some(endpoint))?;
    Ok(stream)
}

fn connect_and_authenticate(
    shared: &Shared,
    endpoint: SocketAddr,
    expected_peer: Option<&PeerState>,
) -> Result<(StreamOwned<ClientConnection, TcpStream>, AuthenticatedPeer), NetworkError> {
    let tcp = TcpStream::connect_timeout(&endpoint, CONNECT_TIMEOUT)?;
    configure_stream(&tcp)?;
    let provider = rustls::crypto::ring::default_provider();
    let observed_fingerprint = Arc::new(RwLock::new(None));
    let verifier: Arc<dyn ServerCertVerifier> = match expected_peer {
        Some(peer) => Arc::new(PinnedCertificateVerifier {
            expected_fingerprint: peer.certificate_fingerprint.clone(),
            algorithms: provider.signature_verification_algorithms,
        }),
        None => Arc::new(RecordingCertificateVerifier {
            observed_fingerprint: Arc::clone(&observed_fingerprint),
            algorithms: provider.signature_verification_algorithms,
        }),
    };
    let client_config = ClientConfig::builder_with_provider(provider.into())
        .with_protocol_versions(&[&version::TLS13])
        .map_err(tls_error)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    let server_name = ServerName::try_from("tossit.local")
        .map_err(|error| NetworkError::Tls(error.to_string()))?;
    let connection =
        ClientConnection::new(Arc::new(client_config), server_name).map_err(tls_error)?;
    let mut stream = StreamOwned::new(connection, tcp);

    let WireFrame::Challenge {
        protocol_version,
        server_nonce,
        certificate_fingerprint,
    } = read_frame(&mut stream)?
    else {
        return Err(NetworkError::InvalidWire("expected challenge".to_owned()));
    };
    validate_certificate_fingerprint(&certificate_fingerprint)?;
    if protocol_version != PROTOCOL_VERSION {
        return Err(NetworkError::PeerIdentityMismatch);
    }
    if let Some(peer) = expected_peer {
        if certificate_fingerprint != peer.certificate_fingerprint {
            return Err(NetworkError::PeerIdentityMismatch);
        }
    } else if observed_fingerprint
        .read()
        .expect("observed certificate lock")
        .as_deref()
        != Some(certificate_fingerprint.as_str())
    {
        return Err(NetworkError::PeerIdentityMismatch);
    }
    let client_nonce = random_nonce()?;
    let proof = client_proof_bytes(
        &server_nonce,
        &client_nonce,
        &certificate_fingerprint,
        &shared.identity_summary.peer_id,
        &shared.certificate_fingerprint,
        shared.listening_port,
    );
    write_frame(
        &mut stream,
        &WireFrame::IdentityProof {
            peer_id: shared.identity_summary.peer_id.clone(),
            display_id: shared.identity_summary.display_id.clone(),
            nickname: Some(shared.nickname.read().expect("nickname lock").clone()),
            avatar_hash: shared
                .avatar
                .read()
                .expect("avatar lock")
                .as_ref()
                .map(|avatar| avatar.hash.clone()),
            public_key: shared.identity_summary.public_key.clone(),
            certificate_fingerprint: shared.certificate_fingerprint.clone(),
            listening_port: shared.listening_port,
            client_nonce: client_nonce.clone(),
            signature: shared.identity.sign(&proof),
        },
    )?;

    let (peer_id, display_id, nickname, avatar_hash, public_key, signature) =
        match read_frame(&mut stream)? {
            WireFrame::IdentityAccepted {
                peer_id,
                display_id,
                nickname,
                avatar_hash,
                public_key,
                signature,
            } => (
                peer_id,
                display_id,
                nickname,
                avatar_hash,
                public_key,
                signature,
            ),
            WireFrame::Rejected { reason } => return Err(NetworkError::PeerRejected(reason)),
            _ => {
                return Err(NetworkError::InvalidWire(
                    "expected device identity acceptance".to_owned(),
                ));
            }
        };
    verify_peer_identity(&peer_id, &display_id, &public_key)?;
    if let Some(peer) = expected_peer {
        if peer_id != peer.peer_id || public_key != peer.public_key {
            return Err(NetworkError::PeerIdentityMismatch);
        }
    }
    let accepted_proof = server_proof_bytes(
        &server_nonce,
        &client_nonce,
        &certificate_fingerprint,
        &shared.identity_summary.peer_id,
        &peer_id,
    );
    DeviceIdentity::verify(&public_key, &accepted_proof, &signature)?;
    Ok((
        stream,
        AuthenticatedPeer {
            peer_id,
            display_id,
            nickname,
            avatar_hash,
            public_key,
            certificate_fingerprint,
        },
    ))
}

fn send_attachment_stream(
    shared: &Shared,
    peer: &PeerState,
    endpoint: SocketAddr,
    envelope: Envelope,
    prepared: &PreparedAttachment,
    message_id: &str,
) -> Result<(), NetworkError> {
    let mut stream = connect_authenticated(shared, peer, endpoint)?;
    stream.sock.set_read_timeout(Some(TRANSFER_TIMEOUT))?;
    stream.sock.set_write_timeout(Some(TRANSFER_TIMEOUT))?;
    write_frame(
        &mut stream,
        &WireFrame::TransferOffer {
            envelope: envelope.clone(),
        },
    )?;
    let transfer_id = match read_frame(&mut stream)? {
        WireFrame::TransferReady { transfer_id } => transfer_id,
        WireFrame::Rejected { reason } => return Err(NetworkError::PeerRejected(reason)),
        _ => {
            return Err(NetworkError::InvalidWire(
                "expected attachment readiness".to_owned(),
            ));
        }
    };
    if transfer_id != prepared.transfer_id {
        return Err(NetworkError::InvalidWire(
            "attachment transfer identifier did not match".to_owned(),
        ));
    }

    let mut source = File::open(&prepared.path)?;
    let mut buffer = vec![0_u8; TRANSFER_CHUNK_BYTES];
    let mut hasher = Sha256::new();
    let mut transferred = 0_u64;
    loop {
        if delivery_is_cancelled(shared, message_id) {
            return Err(NetworkError::TransferCancelled);
        }
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        stream.write_all(&buffer[..read])?;
        hasher.update(&buffer[..read]);
        transferred += read as u64;
        update_transferred_bytes(shared, message_id, transferred);
    }
    if delivery_is_cancelled(shared, message_id) {
        return Err(NetworkError::TransferCancelled);
    }
    if transferred != prepared.byte_size {
        return Err(NetworkError::Attachment(format!(
            "attachment changed during transfer; expected {} bytes, read {transferred}",
            prepared.byte_size
        )));
    }
    stream.flush()?;
    write_frame(
        &mut stream,
        &WireFrame::TransferDigest {
            transfer_id: prepared.transfer_id.clone(),
            sha256: hex::encode_upper(hasher.finalize()),
        },
    )?;
    read_matching_acknowledgement(&mut stream, peer, message_id)
}

fn read_matching_acknowledgement(
    stream: &mut impl Read,
    peer: &PeerState,
    sent_message_id: &str,
) -> Result<(), NetworkError> {
    let acknowledgement = match read_frame(stream)? {
        WireFrame::Message { envelope } => envelope,
        WireFrame::Rejected { reason } => return Err(NetworkError::PeerRejected(reason)),
        _ => return Err(NetworkError::InvalidAcknowledgement),
    };
    acknowledgement
        .validate()
        .map_err(|error| NetworkError::InvalidWire(error.to_string()))?;
    match acknowledgement.payload {
        Payload::Acknowledgement {
            acknowledged_message_id,
        } if acknowledged_message_id == sent_message_id
            && acknowledgement.sender_peer_id == peer.peer_id =>
        {
            Ok(())
        }
        _ => Err(NetworkError::InvalidAcknowledgement),
    }
}

struct PreparedAttachment {
    transfer_id: String,
    path: PathBuf,
    preview_path: Option<PathBuf>,
    file_name: String,
    media_type: String,
    byte_size: u64,
}

fn prepare_outgoing_attachment(
    shared: &Shared,
    source_path: &Path,
    kind: AttachmentKind,
    preferred_file_name: Option<&str>,
) -> Result<PreparedAttachment, NetworkError> {
    let metadata = fs::metadata(source_path)?;
    if !metadata.is_file() {
        return Err(NetworkError::Attachment(
            "selected item is not a file".to_owned(),
        ));
    }
    if metadata.len() > MAX_ATTACHMENT_BYTES {
        return Err(NetworkError::Attachment(format!(
            "file is {} bytes; maximum is {MAX_ATTACHMENT_BYTES}",
            metadata.len()
        )));
    }
    let fallback_name = source_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("attachment");
    let file_name = safe_file_name(preferred_file_name.unwrap_or(fallback_name), kind);
    let inferred = infer::get_from_path(source_path)
        .map_err(|error| NetworkError::Attachment(error.to_string()))?;
    let media_type = inferred
        .as_ref()
        .map_or("application/octet-stream", |kind| kind.mime_type())
        .to_owned();
    if kind == AttachmentKind::Image && !is_previewable_image_media_type(&media_type) {
        return Err(NetworkError::Attachment(
            "该图片格式暂不支持预览，请选择 JPEG、PNG、WebP、GIF 或 BMP".to_owned(),
        ));
    }

    let transfer_id = Uuid::new_v4().to_string();
    let destination = shared
        .attachment_dir
        .join("outgoing")
        .join(format!("{transfer_id}-{file_name}"));
    ensure_free_space(
        destination.parent().expect("outgoing attachment parent"),
        metadata.len(),
    )?;
    let copied = fs::copy(source_path, &destination)?;
    if copied != metadata.len() {
        return Err(NetworkError::Attachment(
            "file changed while it was being prepared".to_owned(),
        ));
    }
    set_owner_only(&destination)?;
    let preview_path = match kind {
        AttachmentKind::Image => Some(generate_thumbnail(shared, &destination)?),
        AttachmentKind::File => None,
    };
    Ok(PreparedAttachment {
        transfer_id,
        path: destination,
        preview_path,
        file_name,
        media_type,
        byte_size: copied,
    })
}

fn ensure_free_space(directory: &Path, required_bytes: u64) -> Result<(), NetworkError> {
    let available = fs2::available_space(directory)?;
    let required = required_bytes.saturating_add(STORAGE_HEADROOM_BYTES);
    if available < required {
        return Err(NetworkError::Attachment(format!(
            "存储空间不足，还需要至少 {} MB 可用空间",
            required.div_ceil(1024 * 1024)
        )));
    }
    Ok(())
}

fn received_storage_summary(messages: &[ChatMessage]) -> StorageSummary {
    let mut summary = StorageSummary::default();
    let mut counted_paths = HashSet::new();
    for message in messages {
        if message.direction != MessageDirection::Incoming {
            continue;
        }
        let ChatContent::Attachment { attachment } = &message.content else {
            continue;
        };
        if attachment
            .local_path
            .as_deref()
            .is_some_and(|path| Path::new(path).is_file())
        {
            summary.received_file_count += 1;
        }
        for path in [
            attachment.local_path.as_deref(),
            attachment.preview_path.as_deref(),
        ]
        .into_iter()
        .flatten()
        .map(PathBuf::from)
        {
            if counted_paths.insert(path.clone()) {
                summary.received_bytes = summary
                    .received_bytes
                    .saturating_add(fs::metadata(path).map_or(0, |metadata| metadata.len()));
            }
        }
    }
    summary
}

fn remove_managed_file(path: &Path, managed_directory: &Path) -> Result<(), NetworkError> {
    if !path.starts_with(managed_directory) {
        return Ok(());
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(NetworkError::Io(error)),
    }
}

fn remove_message_files(shared: &Shared, message: &ChatMessage) {
    let ChatContent::Attachment { attachment } = &message.content else {
        return;
    };
    let managed_directories = [
        shared.attachment_dir.join("incoming"),
        shared.attachment_dir.join("outgoing"),
        shared.attachment_dir.join("previews"),
    ];
    let paths = [
        attachment.local_path.as_deref(),
        attachment.preview_path.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(PathBuf::from)
    .collect::<HashSet<_>>();

    for path in paths {
        for directory in &managed_directories {
            if let Err(error) = remove_managed_file(&path, directory) {
                eprintln!("TossIt could not remove deleted attachment: {error}");
            }
        }
    }
}

fn generate_thumbnail(shared: &Shared, source: &Path) -> Result<PathBuf, NetworkError> {
    let mut reader = image::ImageReader::open(source)
        .and_then(|reader| reader.with_guessed_format())
        .map_err(|error| {
            NetworkError::Attachment(format!("image preview could not be opened: {error}"))
        })?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(20_000);
    limits.max_image_height = Some(20_000);
    limits.max_alloc = Some(192 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().map_err(|error| {
        NetworkError::Attachment(format!("image preview could not be created: {error}"))
    })?;
    let thumbnail = image.thumbnail(THUMBNAIL_MAX_EDGE, THUMBNAIL_MAX_EDGE);
    let destination = shared
        .attachment_dir
        .join("previews")
        .join(format!("{}.jpg", Uuid::new_v4()));
    thumbnail
        .save_with_format(&destination, image::ImageFormat::Jpeg)
        .map_err(|error| {
            NetworkError::Attachment(format!("image preview could not be saved: {error}"))
        })?;
    set_owner_only(&destination)?;
    Ok(destination)
}

fn is_previewable_image_media_type(media_type: &str) -> bool {
    matches!(
        media_type,
        "image/jpeg" | "image/png" | "image/webp" | "image/gif" | "image/bmp"
    )
}

fn safe_file_name(value: &str, kind: AttachmentKind) -> String {
    let mut result = String::new();
    for character in value.trim().chars() {
        let character = if character.is_control()
            || matches!(
                character,
                '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*'
            ) {
            '_'
        } else {
            character
        };
        if result.len() + character.len_utf8() > 240 {
            break;
        }
        result.push(character);
    }
    while result.ends_with(['.', ' ']) {
        result.pop();
    }
    let windows_stem = result
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let reserved_windows_name = matches!(windows_stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (windows_stem.len() == 4
            && (windows_stem.starts_with("COM") || windows_stem.starts_with("LPT"))
            && matches!(windows_stem.as_bytes()[3], b'1'..=b'9'));
    if result.is_empty() || matches!(result.as_str(), "." | "..") {
        match kind {
            AttachmentKind::Image => "图片".to_owned(),
            AttachmentKind::File => "文件".to_owned(),
        }
    } else if reserved_windows_name {
        format!("_{result}")
    } else {
        result
    }
}

fn update_transferred_bytes(shared: &Shared, message_id: &str, transferred_bytes: u64) {
    let mut state = shared.state.write().expect("network state lock");
    if let Some(ChatMessage {
        content: ChatContent::Attachment { attachment },
        ..
    }) = state
        .messages
        .iter_mut()
        .find(|message| message.message_id == message_id)
    {
        attachment.transferred_bytes = transferred_bytes.min(attachment.byte_size);
    }
}

fn complete_received_attachment(
    shared: &Shared,
    message_id: &str,
    local_path: &Path,
    preview_path: Option<&Path>,
    byte_size: u64,
) -> Result<(), NetworkError> {
    let mut state = shared.state.write().expect("network state lock");
    let completed = if let Some(message) = state
        .messages
        .iter_mut()
        .find(|message| message.message_id == message_id)
    {
        message.delivery = DeliveryState::Received;
        if let ChatContent::Attachment { attachment } = &mut message.content {
            attachment.transferred_bytes = byte_size;
            attachment.local_path = Some(path_string(local_path));
            attachment.preview_path = preview_path.map(path_string);
        }
        Some(message.clone())
    } else {
        None
    };
    drop(state);
    if let Some(message) = completed {
        persist_message(shared, &message)?;
    }
    Ok(())
}

fn mark_delivery(shared: &Shared, message_id: &str, delivery: DeliveryState) {
    let mut state = shared.state.write().expect("network state lock");
    let changed = if let Some(message) = state
        .messages
        .iter_mut()
        .find(|message| message.message_id == message_id)
    {
        message.delivery = delivery;
        Some(message.clone())
    } else {
        None
    };
    drop(state);
    if let Some(message) = changed {
        if let Err(error) = persist_message(shared, &message) {
            eprintln!("TossIt could not persist delivery state: {error}");
        }
    }
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn set_owner_only(path: &Path) -> Result<(), io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[derive(Debug)]
struct RecordingCertificateVerifier {
    observed_fingerprint: Arc<RwLock<Option<String>>>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for RecordingCertificateVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fingerprint = hex::encode_upper(Sha256::digest(end_entity.as_ref()));
        *self
            .observed_fingerprint
            .write()
            .expect("observed certificate lock") = Some(fingerprint);
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is disabled".to_owned()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[derive(Debug)]
struct PinnedCertificateVerifier {
    expected_fingerprint: String,
    algorithms: WebPkiSupportedAlgorithms,
}

fn validate_certificate_fingerprint(value: &str) -> Result<(), NetworkError> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(NetworkError::InvalidWire(
            "certificate fingerprint is invalid".to_owned(),
        ))
    }
}

impl ServerCertVerifier for PinnedCertificateVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let received = hex::encode_upper(Sha256::digest(end_entity.as_ref()));
        if received == self.expected_fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "TossIt certificate fingerprint mismatch".to_owned(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is disabled".to_owned()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

fn verify_peer_identity(
    peer_id: &str,
    display_id: &str,
    public_key: &str,
) -> Result<(), NetworkError> {
    if DeviceIdentity::peer_id_for_public_key(public_key)? != peer_id
        || display_id_for_peer(peer_id).as_deref() != Some(display_id)
    {
        return Err(NetworkError::PeerIdentityMismatch);
    }
    Ok(())
}

fn display_id_for_peer(peer_id: &str) -> Option<String> {
    if peer_id.len() != 64 || !peer_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(
        peer_id.as_bytes()[..12]
            .chunks(4)
            .map(|chunk| std::str::from_utf8(chunk).expect("hex Peer ID is UTF-8"))
            .collect::<Vec<_>>()
            .join("-"),
    )
}

fn conversation_id(network_id: &str, local_peer_id: &str, remote_peer_id: &str) -> String {
    let (first, second) = if local_peer_id <= remote_peer_id {
        (local_peer_id, remote_peer_id)
    } else {
        (remote_peer_id, local_peer_id)
    };
    hex::encode_upper(Sha256::digest(format!(
        "tossit-conversation-v2\0{network_id}\0{first}\0{second}"
    )))
}

fn verification_code(local_peer_id: &str, remote_peer_id: &str) -> String {
    let (first, second) = if local_peer_id <= remote_peer_id {
        (local_peer_id, remote_peer_id)
    } else {
        (remote_peer_id, local_peer_id)
    };
    let digest = Sha256::digest(format!("{VERIFICATION_CODE_CONTEXT}\0{first}\0{second}"));
    let value = u32::from_be_bytes([0, digest[0], digest[1], digest[2]]) % 1_000_000;
    let digits = format!("{value:06}");
    format!("{} {}", &digits[..3], &digits[3..])
}

fn client_proof_bytes(
    server_nonce: &str,
    client_nonce: &str,
    server_certificate_fingerprint: &str,
    client_peer_id: &str,
    client_certificate_fingerprint: &str,
    client_listening_port: u16,
) -> Vec<u8> {
    format!(
        "{CLIENT_PROOF_CONTEXT}\0{server_nonce}\0{client_nonce}\0{server_certificate_fingerprint}\0{client_peer_id}\0{client_certificate_fingerprint}\0{client_listening_port}"
    )
    .into_bytes()
}

fn server_proof_bytes(
    server_nonce: &str,
    client_nonce: &str,
    certificate_fingerprint: &str,
    client_peer_id: &str,
    server_peer_id: &str,
) -> Vec<u8> {
    format!(
        "{SERVER_PROOF_CONTEXT}\0{server_nonce}\0{client_nonce}\0{certificate_fingerprint}\0{client_peer_id}\0{server_peer_id}"
    )
    .into_bytes()
}

fn random_nonce() -> Result<String, NetworkError> {
    let mut nonce = [0_u8; 32];
    getrandom::fill(&mut nonce)
        .map_err(|error| NetworkError::InvalidWire(format!("secure random failed: {error}")))?;
    Ok(hex::encode(nonce))
}

fn configure_stream(stream: &TcpStream) -> Result<(), io::Error> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    stream.set_nodelay(true)
}

fn write_frame(writer: &mut impl Write, frame: &WireFrame) -> Result<(), NetworkError> {
    let payload =
        serde_json::to_vec(frame).map_err(|error| NetworkError::InvalidWire(error.to_string()))?;
    if payload.len() > FRAME_MAX_BYTES {
        return Err(NetworkError::InvalidWire("frame is too large".to_owned()));
    }
    writer.write_all(&(payload.len() as u32).to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

fn read_frame(reader: &mut impl Read) -> Result<WireFrame, NetworkError> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > FRAME_MAX_BYTES {
        return Err(NetworkError::InvalidWire("invalid frame length".to_owned()));
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload)?;
    serde_json::from_slice(&payload).map_err(|error| NetworkError::InvalidWire(error.to_string()))
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn tls_error(error: impl std::fmt::Display) -> NetworkError {
    NetworkError::Tls(error.to_string())
}

fn discovery_error(error: impl std::fmt::Display) -> NetworkError {
    NetworkError::Discovery(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tossit_identity::IDENTITY_FILE_NAME;

    fn identity(directory: &tempfile::TempDir, name: &str) -> Arc<DeviceIdentity> {
        Arc::new(
            DeviceIdentity::load_or_create(directory.path().join(name).join(IDENTITY_FILE_NAME))
                .expect("create test identity"),
        )
    }

    fn direct_peer(node: &NetworkNode) -> PeerState {
        let shared = &node.runtime.shared;
        let avatar_hash = shared
            .avatar
            .read()
            .expect("avatar lock")
            .as_ref()
            .map(|avatar| avatar.hash.clone());
        PeerState {
            peer_id: shared.identity_summary.peer_id.clone(),
            display_id: shared.identity_summary.display_id.clone(),
            alias: format!("TossIt {}", shared.identity_summary.display_id),
            public_key: shared.identity_summary.public_key.clone(),
            certificate_fingerprint: shared.certificate_fingerprint.clone(),
            endpoint: Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                shared.listening_port,
            )),
            service_fullnames: HashSet::new(),
            last_seen_unix_ms: unix_time_ms(),
            trust_state: TrustState::Discovered,
            avatar_hash,
            avatar_path: None,
        }
    }

    fn start_node(directory: &tempfile::TempDir, name: &str) -> NetworkNode {
        let data_dir = directory.path().join(name);
        let node = NetworkNode::start(
            identity(directory, name),
            format!("Test {name}"),
            data_dir.join("attachments"),
            Store::open(data_dir.join("tossit.sqlite3")).expect("open test store"),
        )
        .expect("start test node");
        node.set_active_network(Some(ActiveNetwork {
            network_id: "TEST-NETWORK".to_owned(),
            display_name: "Test Wi-Fi".to_owned(),
        }))
        .expect("set test network");
        node
    }

    fn pair_direct(first: &NetworkNode, second: &NetworkNode) -> (PeerState, PeerState) {
        let first_peer = direct_peer(first);
        let second_peer = direct_peer(second);
        upsert_discovered_peer(&first.runtime.shared, "test-second", second_peer.clone());
        upsert_discovered_peer(&second.runtime.shared, "test-first", first_peer.clone());
        first
            .set_peer_trust(&second_peer.peer_id, TrustState::Trusted)
            .expect("first trusts second");
        second
            .set_peer_trust(&first_peer.peer_id, TrustState::Trusted)
            .expect("second trusts first");
        (first_peer, second_peer)
    }

    #[test]
    fn conversation_id_is_identical_from_both_sides() {
        assert_eq!(
            conversation_id("N1", "A", "B"),
            conversation_id("N1", "B", "A")
        );
        assert_ne!(
            conversation_id("N1", "A", "B"),
            conversation_id("N2", "A", "B")
        );
        assert_ne!(
            conversation_id("N1", "A", "B"),
            conversation_id("N1", "A", "C")
        );
    }

    #[test]
    fn verification_code_is_identical_on_both_devices() {
        let first = "AAAABBBBCCCC";
        let second = "DDDDEEEEFFFF";
        assert_eq!(
            verification_code(first, second),
            verification_code(second, first)
        );
        assert_eq!(verification_code(first, second).len(), 7);
    }

    #[test]
    fn untrusted_peer_cannot_send_a_message() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let first = start_node(&directory, "untrusted-first");
        let second = start_node(&directory, "untrusted-second");
        let second_peer = direct_peer(&second);
        upsert_discovered_peer(&first.runtime.shared, "test-second", second_peer.clone());

        assert!(matches!(
            first.send_text(&second_peer.peer_id, "must be confirmed"),
            Err(NetworkError::PeerUntrusted)
        ));
        assert!(first.snapshot().messages.is_empty());
        assert!(second.snapshot().messages.is_empty());
    }

    #[test]
    fn missing_active_wifi_keeps_history_available_but_blocks_sending() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let first = start_node(&directory, "offline-first");
        let second = start_node(&directory, "offline-second");
        let (_first_peer, second_peer) = pair_direct(&first, &second);
        first
            .set_active_network(None)
            .expect("clear current network");

        assert!(matches!(
            first.send_text(&second_peer.peer_id, "must wait for Wi-Fi"),
            Err(NetworkError::NoActiveNetwork)
        ));
        let snapshot = first.snapshot();
        assert!(snapshot.active_network.is_none());
        assert!(snapshot.messages.is_empty());
        assert!(snapshot.network_spaces.is_empty());
    }

    #[test]
    fn trusted_offline_peer_queues_text_and_delivers_after_rediscovery() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let first = start_node(&directory, "queued-first");
        let second = start_node(&directory, "queued-second");
        let (_first_peer, second_peer) = pair_direct(&first, &second);
        remove_discovered_service(&first.runtime.shared, "test-second");
        assert!(!first.snapshot().peers[0].is_online);

        let queued = first
            .send_text(&second_peer.peer_id, "wake up when ready")
            .expect("queue text for offline trusted peer");
        assert_eq!(queued.delivery, DeliveryState::Sending);
        assert!(second.snapshot().messages.is_empty());

        upsert_discovered_peer(
            &first.runtime.shared,
            "test-second-returned",
            direct_peer(&second),
        );
        retry_pending_deliveries_once(&first.runtime.shared);

        assert_eq!(
            first.snapshot().messages[0].delivery,
            DeliveryState::Delivered
        );
        assert_eq!(second.snapshot().messages.len(), 1);
        assert_eq!(
            second.snapshot().messages[0].content,
            ChatContent::Text {
                text: "wake up when ready".to_owned()
            }
        );
    }

    #[test]
    fn queued_text_survives_sender_restart() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let second = start_node(&directory, "queued-restart-second");
        let second_peer_id = second.runtime.shared.identity_summary.peer_id.clone();
        {
            let first = start_node(&directory, "queued-restart-first");
            let (_first_peer, second_peer) = pair_direct(&first, &second);
            remove_discovered_service(&first.runtime.shared, "test-second");
            let queued = first
                .send_text(&second_peer.peer_id, "survives queued restart")
                .expect("queue before restart");
            assert_eq!(queued.delivery, DeliveryState::Sending);
        }

        let first = start_node(&directory, "queued-restart-first");
        assert_eq!(
            first.snapshot().messages[0].delivery,
            DeliveryState::Sending
        );
        upsert_discovered_peer(
            &first.runtime.shared,
            "test-second-returned",
            direct_peer(&second),
        );
        retry_pending_deliveries_once(&first.runtime.shared);

        assert_eq!(
            first.snapshot().messages[0].delivery,
            DeliveryState::Delivered
        );
        assert_eq!(first.snapshot().messages[0].peer_id, second_peer_id);
        assert_eq!(second.snapshot().messages.len(), 1);
    }

    #[test]
    fn trusted_offline_peer_queues_file_and_delivers_after_rediscovery() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let first = start_node(&directory, "queued-file-first");
        let second = start_node(&directory, "queued-file-second");
        let (_first_peer, second_peer) = pair_direct(&first, &second);
        let source = directory.path().join("queued-file.txt");
        fs::write(&source, b"queued attachment contents").expect("write queued fixture");
        remove_discovered_service(&first.runtime.shared, "test-second");

        let queued = first
            .send_attachment(
                &second_peer.peer_id,
                &source,
                AttachmentKind::File,
                Some("queued-file.txt"),
            )
            .expect("queue file for offline trusted peer");
        assert_eq!(queued.delivery, DeliveryState::Sending);
        assert!(second.snapshot().messages.is_empty());

        upsert_discovered_peer(
            &first.runtime.shared,
            "test-second-returned",
            direct_peer(&second),
        );
        retry_pending_deliveries_once(&first.runtime.shared);

        assert_eq!(
            first.snapshot().messages[0].delivery,
            DeliveryState::Delivered
        );
        let received = second
            .snapshot()
            .messages
            .pop()
            .expect("received queued file");
        let ChatContent::Attachment { attachment } = received.content else {
            panic!("expected queued attachment");
        };
        assert_eq!(
            fs::read(attachment.local_path.expect("received file path")).unwrap(),
            b"queued attachment contents"
        );
    }

    #[test]
    fn queued_file_can_be_cancelled_and_manually_retried() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let first = start_node(&directory, "cancel-file-first");
        let second = start_node(&directory, "cancel-file-second");
        let (_first_peer, second_peer) = pair_direct(&first, &second);
        let source = directory.path().join("cancel-file.txt");
        fs::write(&source, b"cancel then retry").expect("write fixture");
        remove_discovered_service(&first.runtime.shared, "test-second");

        let queued = first
            .send_attachment(
                &second_peer.peer_id,
                &source,
                AttachmentKind::File,
                Some("cancel-file.txt"),
            )
            .expect("queue attachment");
        let cancelled = first
            .cancel_message(&queued.message_id)
            .expect("cancel queued attachment");
        assert_eq!(cancelled.messages[0].delivery, DeliveryState::Failed);

        upsert_discovered_peer(
            &first.runtime.shared,
            "test-second-returned",
            direct_peer(&second),
        );
        retry_pending_deliveries_once(&first.runtime.shared);
        assert!(second.snapshot().messages.is_empty());

        let retried = first
            .retry_message(&queued.message_id)
            .expect("retry cancelled attachment");
        assert_eq!(retried.delivery, DeliveryState::Delivered);
        assert_eq!(second.snapshot().messages.len(), 1);
    }

    #[test]
    fn free_space_preflight_rejects_impossible_requirement() {
        let directory = tempfile::tempdir().expect("create temp directory");
        assert!(matches!(
            ensure_free_space(directory.path(), u64::MAX),
            Err(NetworkError::Attachment(message)) if message.contains("存储空间不足")
        ));
    }

    #[test]
    fn older_history_is_loaded_without_replacing_current_messages() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let first = start_node(&directory, "history-first");
        let second = start_node(&directory, "history-second");
        let (_first_peer, second_peer) = pair_direct(&first, &second);
        let conversation_id = conversation_id(
            "TEST-NETWORK",
            &first.runtime.shared.identity_summary.peer_id,
            &second_peer.peer_id,
        );
        let messages = [("history-a", 100), ("history-b", 200), ("history-c", 300)]
            .into_iter()
            .map(|(message_id, created_at_unix_ms)| ChatMessage {
                message_id: message_id.to_owned(),
                network_id: "TEST-NETWORK".to_owned(),
                conversation_id: conversation_id.clone(),
                peer_id: second_peer.peer_id.clone(),
                direction: MessageDirection::Incoming,
                delivery: DeliveryState::Received,
                content: ChatContent::Text {
                    text: message_id.to_owned(),
                },
                created_at_unix_ms,
                is_read: true,
            })
            .collect::<Vec<_>>();
        for message in &messages {
            persist_message(&first.runtime.shared, message).expect("persist history fixture");
        }
        first
            .runtime
            .shared
            .state
            .write()
            .expect("network state lock")
            .messages
            .push(messages[2].clone());

        let first_page = first
            .load_older_messages("TEST-NETWORK", &second_peer.peer_id, 300, "history-c", 1)
            .expect("load first history page");
        assert_eq!(first_page.loaded, 1);
        assert!(first_page.has_more);
        assert_eq!(first_page.snapshot.messages[0].message_id, "history-b");

        let second_page = first
            .load_older_messages("TEST-NETWORK", &second_peer.peer_id, 200, "history-b", 1)
            .expect("load final history page");
        assert_eq!(second_page.loaded, 1);
        assert!(!second_page.has_more);
        assert_eq!(second_page.snapshot.messages[0].message_id, "history-a");
        assert_eq!(second_page.snapshot.messages.len(), 3);
    }

    #[test]
    fn received_files_can_be_cleared_without_removing_chat_history() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let first = start_node(&directory, "cleanup-first");
        let second = start_node(&directory, "cleanup-second");
        let (_first_peer, second_peer) = pair_direct(&first, &second);
        let source = directory.path().join("cleanup.txt");
        fs::write(&source, b"cleanup fixture").expect("write fixture");

        first
            .send_attachment(
                &second_peer.peer_id,
                &source,
                AttachmentKind::File,
                Some("cleanup.txt"),
            )
            .expect("send fixture");
        let received_path = match &second.snapshot().messages[0].content {
            ChatContent::Attachment { attachment } => attachment
                .local_path
                .as_deref()
                .map(PathBuf::from)
                .expect("received file path"),
            ChatContent::Text { .. } => panic!("expected attachment"),
        };
        assert!(received_path.is_file());
        assert_eq!(
            second
                .storage_summary()
                .expect("storage summary")
                .received_file_count,
            1
        );

        let summary = second.clear_received_files().expect("clear received files");
        assert_eq!(summary.received_file_count, 0);
        assert!(!received_path.exists());
        assert_eq!(second.snapshot().messages.len(), 1);
        let ChatContent::Attachment { attachment } = &second.snapshot().messages[0].content else {
            panic!("expected attachment history");
        };
        assert!(attachment.local_path.is_none());
    }

    #[test]
    fn deleting_attachment_removes_only_the_local_copy_and_history() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let first = start_node(&directory, "delete-first");
        let second = start_node(&directory, "delete-second");
        let (_first_peer, second_peer) = pair_direct(&first, &second);
        let source = directory.path().join("delete.txt");
        fs::write(&source, b"delete fixture").expect("write fixture");

        let sent = first
            .send_attachment(
                &second_peer.peer_id,
                &source,
                AttachmentKind::File,
                Some("delete.txt"),
            )
            .expect("send fixture");
        let outgoing_path = match &first.snapshot().messages[0].content {
            ChatContent::Attachment { attachment } => PathBuf::from(
                attachment
                    .local_path
                    .as_deref()
                    .expect("outgoing file path"),
            ),
            ChatContent::Text { .. } => panic!("expected outgoing attachment"),
        };
        let incoming = second.snapshot().messages[0].clone();
        let incoming_path = match &incoming.content {
            ChatContent::Attachment { attachment } => PathBuf::from(
                attachment
                    .local_path
                    .as_deref()
                    .expect("incoming file path"),
            ),
            ChatContent::Text { .. } => panic!("expected incoming attachment"),
        };

        second
            .delete_message(&incoming.message_id)
            .expect("delete received message");
        assert!(second.snapshot().messages.is_empty());
        assert!(!incoming_path.exists());
        assert_eq!(first.snapshot().messages.len(), 1);
        assert!(outgoing_path.is_file());

        first
            .delete_message(&sent.message_id)
            .expect("delete sent message");
        assert!(first.snapshot().messages.is_empty());
        assert!(!outgoing_path.exists());
        assert!(first
            .runtime
            .shared
            .store
            .load_all_messages()
            .expect("reload first history")
            .is_empty());
        assert!(second
            .runtime
            .shared
            .store
            .load_all_messages()
            .expect("reload second history")
            .is_empty());
    }

    #[test]
    fn manual_endpoint_discovers_both_devices_before_trust() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let first = start_node(&directory, "manual-first");
        let second = start_node(&directory, "manual-second");
        let first_peer_id = first.runtime.shared.identity_summary.peer_id.clone();
        let second_peer_id = second.runtime.shared.identity_summary.peer_id.clone();
        let endpoint = format!("127.0.0.1:{}", second.runtime.shared.listening_port);

        let first_snapshot = first
            .connect_endpoint(&endpoint)
            .expect("connect with manual endpoint");
        let first_view = first_snapshot
            .peers
            .iter()
            .find(|peer| peer.peer_id == second_peer_id)
            .expect("first discovered second");
        let second_snapshot = second.snapshot();
        let second_view = second_snapshot
            .peers
            .iter()
            .find(|peer| peer.peer_id == first_peer_id)
            .expect("second learned first identity");
        assert_eq!(first_view.trust_state, TrustState::Discovered);
        assert_eq!(second_view.trust_state, TrustState::Discovered);
        assert_eq!(first_view.alias, "Test manual-second");
        assert_eq!(second_view.alias, "Test manual-first");
        assert_eq!(first_view.verification_code, second_view.verification_code);
        assert!(first_view.is_online);
        assert!(second_view.is_online);

        first
            .set_peer_trust(&second_peer_id, TrustState::Trusted)
            .expect("first trusts second");
        second
            .set_peer_trust(&first_peer_id, TrustState::Trusted)
            .expect("second trusts first");
        let sent = second
            .send_text(&first_peer_id, "manual route works both ways")
            .expect("send back through authenticated manual route");
        assert_eq!(sent.delivery, DeliveryState::Delivered);
        assert_eq!(first.snapshot().messages.len(), 1);
    }

    #[test]
    fn manual_endpoint_parser_accepts_ip_and_rejects_unsafe_addresses() {
        assert_eq!(
            parse_manual_endpoint("http://192.168.1.8:42318/").expect("parse endpoint"),
            SocketAddr::from(([192, 168, 1, 8], 42318))
        );
        assert!(parse_manual_endpoint("192.168.1.8").is_err());
        assert!(parse_manual_endpoint("0.0.0.0:42318").is_err());
        assert!(parse_manual_endpoint("224.0.0.1:42318").is_err());
        assert!(parse_manual_endpoint("192.168.1.8:0").is_err());
    }

    #[test]
    fn wifi_endpoint_is_not_replaced_by_a_link_local_discovery() {
        let wifi = "192.168.10.176:51975".parse().expect("Wi-Fi endpoint");
        let link_local = "169.254.182.153:51975"
            .parse()
            .expect("link-local endpoint");

        assert!(prefer_discovered_endpoint(Some(link_local), Some(wifi)));
        assert!(!prefer_discovered_endpoint(Some(wifi), Some(link_local)));
    }

    #[test]
    fn receiver_must_also_confirm_the_sender() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let first = start_node(&directory, "one-sided-first");
        let second = start_node(&directory, "one-sided-second");
        let first_peer = direct_peer(&first);
        let second_peer = direct_peer(&second);
        upsert_discovered_peer(&first.runtime.shared, "test-second", second_peer.clone());
        upsert_discovered_peer(&second.runtime.shared, "test-first", first_peer);
        first
            .set_peer_trust(&second_peer.peer_id, TrustState::Trusted)
            .expect("first trusts second");

        assert!(matches!(
            first.send_text(&second_peer.peer_id, "one-sided trust"),
            Err(NetworkError::PeerRejected(_))
        ));
        assert_eq!(first.snapshot().messages.len(), 1);
        assert_eq!(first.snapshot().messages[0].delivery, DeliveryState::Failed);
        assert!(first.snapshot().network_spaces.is_empty());
        assert!(second.snapshot().messages.is_empty());
    }

    #[test]
    fn two_nodes_exchange_encrypted_text_and_acknowledgement() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let first = start_node(&directory, "first");
        let second = start_node(&directory, "second");
        let (_first_peer, second_peer) = pair_direct(&first, &second);

        let sent = first
            .send_text(&second_peer.peer_id, "hello over TLS")
            .expect("send text");
        assert_eq!(sent.delivery, DeliveryState::Delivered);

        let first_snapshot = first.snapshot();
        let second_snapshot = second.snapshot();
        assert_eq!(first_snapshot.messages.len(), 1);
        assert_eq!(second_snapshot.messages.len(), 1);
        assert_eq!(first_snapshot.network_spaces.len(), 1);
        assert_eq!(second_snapshot.network_spaces.len(), 1);
        assert_eq!(
            second_snapshot.messages[0].content,
            ChatContent::Text {
                text: "hello over TLS".to_owned()
            }
        );
        assert_eq!(
            second_snapshot.messages[0].direction,
            MessageDirection::Incoming
        );
        assert_eq!(
            first_snapshot.messages[0].conversation_id,
            second_snapshot.messages[0].conversation_id
        );
    }

    #[test]
    fn trusted_peer_fetches_avatar_over_encrypted_connection() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let first = start_node(&directory, "avatar-first");
        let second = start_node(&directory, "avatar-second");
        let avatar_path = directory.path().join("avatar.jpg");
        image::RgbImage::from_pixel(256, 256, image::Rgb([41, 96, 228]))
            .save(&avatar_path)
            .expect("save test avatar");
        second
            .set_avatar(Some(&avatar_path))
            .expect("set second avatar");
        let (_first_peer, second_peer) = pair_direct(&first, &second);

        let snapshot = first
            .sync_peer_avatar(&second_peer.peer_id)
            .expect("fetch trusted avatar");
        let peer = snapshot
            .peers
            .iter()
            .find(|peer| peer.peer_id == second_peer.peer_id)
            .expect("second peer remains available");
        let cached_path = peer.avatar_path.as_deref().expect("avatar was cached");
        assert_eq!(peer.avatar_hash, second.avatar_hash());
        assert_eq!(
            fs::read(cached_path).unwrap(),
            fs::read(avatar_path).unwrap()
        );
    }

    #[test]
    fn restart_preserves_trusted_peer_message_and_unread_state() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let second_peer_id = {
            let first = start_node(&directory, "restart-first");
            let second = start_node(&directory, "restart-second");
            let (_first_peer, second_peer) = pair_direct(&first, &second);
            first
                .send_text(&second_peer.peer_id, "survives restart")
                .expect("send text");
            assert_eq!(second.snapshot().peers[0].unread_count, 1);
            second_peer.peer_id
        };

        let first = start_node(&directory, "restart-first");
        let snapshot = first.snapshot();
        let peer = snapshot
            .peers
            .iter()
            .find(|peer| peer.peer_id == second_peer_id)
            .expect("restored peer");
        assert_eq!(peer.trust_state, TrustState::Trusted);
        assert!(!peer.is_online);
        assert_eq!(snapshot.messages.len(), 1);
        assert_eq!(snapshot.messages[0].delivery, DeliveryState::Delivered);

        let second = start_node(&directory, "restart-second");
        let snapshot = second.snapshot();
        assert_eq!(snapshot.messages.len(), 1);
        assert_eq!(snapshot.messages[0].direction, MessageDirection::Incoming);
        assert!(!snapshot.messages[0].is_read);
        let sender = snapshot
            .peers
            .iter()
            .find(|peer| peer.peer_id == snapshot.messages[0].peer_id)
            .expect("restored sender peer");
        assert_eq!(sender.unread_count, 1);
        second
            .mark_peer_read(&sender.peer_id, &snapshot.messages[0].network_id)
            .expect("mark conversation read");
        drop(second);
        let second = start_node(&directory, "restart-second");
        assert_eq!(second.snapshot().peers[0].unread_count, 0);
        assert!(second.snapshot().messages[0].is_read);
    }

    #[test]
    fn two_nodes_stream_file_in_chunks_and_preserve_exact_bytes() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let first = start_node(&directory, "file-first");
        let second = start_node(&directory, "file-second");
        let (_first_peer, second_peer) = pair_direct(&first, &second);
        let bytes = (0..(TRANSFER_CHUNK_BYTES * 3 + 517))
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let source = directory.path().join("fixture.bin");
        fs::write(&source, &bytes).expect("write source file");

        let sent = first
            .send_attachment(
                &second_peer.peer_id,
                &source,
                AttachmentKind::File,
                Some("fixture.bin"),
            )
            .expect("send file");
        assert_eq!(sent.delivery, DeliveryState::Delivered);
        let received = second.snapshot().messages.pop().expect("received message");
        assert_eq!(received.delivery, DeliveryState::Received);
        let ChatContent::Attachment { attachment } = received.content else {
            panic!("expected file attachment");
        };
        assert_eq!(attachment.kind, AttachmentKind::File);
        assert_eq!(attachment.transferred_bytes, bytes.len() as u64);
        assert_eq!(attachment.byte_size, bytes.len() as u64);
        assert!(attachment.preview_path.is_none());
        let received_path = attachment.local_path.expect("received file path");
        assert_eq!(fs::read(received_path).expect("read received file"), bytes);
    }

    #[test]
    fn two_nodes_create_preview_before_acknowledging_image() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let first = start_node(&directory, "image-first");
        let second = start_node(&directory, "image-second");
        let (_first_peer, second_peer) = pair_direct(&first, &second);
        let source = directory.path().join("sample.png");
        image::RgbImage::from_pixel(1_440, 1_080, image::Rgb([39, 93, 255]))
            .save(&source)
            .expect("save sample image");

        let sent = first
            .send_attachment(
                &second_peer.peer_id,
                &source,
                AttachmentKind::Image,
                Some("sample.png"),
            )
            .expect("send image");
        assert_eq!(sent.delivery, DeliveryState::Delivered);
        let received = second.snapshot().messages.pop().expect("received message");
        let ChatContent::Attachment { attachment } = received.content else {
            panic!("expected image attachment");
        };
        assert_eq!(attachment.kind, AttachmentKind::Image);
        let preview_path = attachment.preview_path.expect("image preview path");
        let preview = image::open(preview_path).expect("open generated preview");
        assert!(preview.width() <= THUMBNAIL_MAX_EDGE);
        assert!(preview.height() <= THUMBNAIL_MAX_EDGE);
        assert!(attachment.local_path.is_some());
    }

    #[test]
    #[ignore = "requires multicast DNS on an active local interface"]
    fn two_nodes_discover_each_other_over_mdns() {
        let directory = tempfile::tempdir().expect("create temp directory");
        let first = start_node(&directory, "mdns-first");
        let second = start_node(&directory, "mdns-second");
        let first_peer_id = first.runtime.shared.identity_summary.peer_id.clone();
        let second_peer_id = second.runtime.shared.identity_summary.peer_id.clone();
        let deadline = std::time::Instant::now() + Duration::from_secs(8);

        let discovered = loop {
            let first_found = first
                .snapshot()
                .peers
                .iter()
                .any(|peer| peer.peer_id == second_peer_id && peer.is_online);
            let second_found = second
                .snapshot()
                .peers
                .iter()
                .any(|peer| peer.peer_id == first_peer_id && peer.is_online);
            if first_found && second_found {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            thread::sleep(Duration::from_millis(100));
        };

        assert!(
            discovered,
            "two local nodes did not discover each other over mDNS"
        );
        first
            .set_peer_trust(&second_peer_id, TrustState::Trusted)
            .expect("first trusts second");
        second
            .set_peer_trust(&first_peer_id, TrustState::Trusted)
            .expect("second trusts first");
        let sent = first
            .send_text(&second_peer_id, "automatic discovery path")
            .expect("send through discovered endpoint");

        assert_eq!(sent.delivery, DeliveryState::Delivered);
        assert!(second.snapshot().messages.iter().any(|message| {
            message.content
                == ChatContent::Text {
                    text: "automatic discovery path".to_owned(),
                }
        }));
    }
}
