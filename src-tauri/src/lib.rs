mod apple_connectivity;
mod device_profile;

use std::sync::Arc;
use tauri::Manager;
use tossit_identity::{DeviceIdentity, IDENTITY_FILE_NAME};
use tossit_network::{
    AttachmentKind, ChatMessage, HistoryPage, NetworkNode, NetworkSnapshot, StorageSummary,
    TrustState,
};
use tossit_storage::{Store, DATABASE_FILE_NAME};

const IDENTITY_KEYCHAIN_SERVICE: &str = "cc.mlxb.tossit.device-identity";
const IDENTITY_KEYCHAIN_ACCOUNT: &str = "identity-v1";

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalDeviceIdentity {
    peer_id: String,
    display_id: String,
    public_key: String,
    nickname: String,
    avatar_hash: Option<String>,
    avatar_path: Option<String>,
}

fn local_device_identity(
    identity: &DeviceIdentity,
    profile: &device_profile::DeviceProfileState,
) -> LocalDeviceIdentity {
    let identity = identity.summary();
    let profile = profile.snapshot();
    LocalDeviceIdentity {
        peer_id: identity.peer_id,
        display_id: identity.display_id,
        public_key: identity.public_key,
        nickname: profile.nickname,
        avatar_hash: profile.avatar_hash,
        avatar_path: profile.avatar_path,
    }
}

#[tauri::command]
fn app_status() -> tossit_core::ApplicationStatus {
    tossit_core::application_status()
}

#[tauri::command]
fn device_identity(
    identity: tauri::State<'_, Arc<DeviceIdentity>>,
    profile: tauri::State<'_, device_profile::DeviceProfileState>,
) -> LocalDeviceIdentity {
    local_device_identity(&identity, &profile)
}

#[tauri::command]
fn set_device_nickname(
    identity: tauri::State<'_, Arc<DeviceIdentity>>,
    profile: tauri::State<'_, device_profile::DeviceProfileState>,
    network: tauri::State<'_, NetworkNode>,
    nickname: String,
) -> Result<LocalDeviceIdentity, String> {
    let nickname = device_profile::normalize_nickname(&nickname)?;
    let previous = profile.snapshot();
    network
        .set_nickname(&nickname)
        .map_err(|error| error.to_string())?;
    if let Err(error) = profile.save(&nickname, previous.avatar_hash.as_deref()) {
        let _ = network.set_nickname(&previous.nickname);
        return Err(error);
    }
    Ok(local_device_identity(&identity, &profile))
}

#[tauri::command]
async fn set_device_avatar(
    identity: tauri::State<'_, Arc<DeviceIdentity>>,
    profile: tauri::State<'_, device_profile::DeviceProfileState>,
    network: tauri::State<'_, NetworkNode>,
    path: tauri_plugin_fs::FilePath,
) -> Result<LocalDeviceIdentity, String> {
    let source_path = path.into_path().map_err(|error| error.to_string())?;
    let identity = Arc::clone(identity.inner());
    let network = network.inner().clone();
    let previous = profile.snapshot();
    let (hash, avatar_path) = profile.prepare_avatar(&source_path)?;
    if previous.avatar_hash.as_deref() == Some(hash.as_str()) {
        return Ok(local_device_identity(&identity, &profile));
    }
    if let Err(error) = network.set_avatar(Some(&avatar_path)) {
        profile.remove_avatar_file(&hash);
        return Err(error.to_string());
    }
    if let Err(error) = profile.save(&previous.nickname, Some(&hash)) {
        let rollback = previous.avatar_path.as_deref().map(std::path::Path::new);
        let _ = network.set_avatar(rollback);
        profile.remove_avatar_file(&hash);
        return Err(error);
    }
    if let Some(previous_hash) = previous.avatar_hash {
        profile.remove_avatar_file(&previous_hash);
    }
    Ok(local_device_identity(&identity, &profile))
}

#[tauri::command]
fn remove_device_avatar(
    identity: tauri::State<'_, Arc<DeviceIdentity>>,
    profile: tauri::State<'_, device_profile::DeviceProfileState>,
    network: tauri::State<'_, NetworkNode>,
) -> Result<LocalDeviceIdentity, String> {
    let previous = profile.snapshot();
    if previous.avatar_hash.is_none() {
        return Ok(local_device_identity(&identity, &profile));
    }
    network
        .set_avatar(None)
        .map_err(|error| error.to_string())?;
    if let Err(error) = profile.save(&previous.nickname, None) {
        let rollback = previous.avatar_path.as_deref().map(std::path::Path::new);
        let _ = network.set_avatar(rollback);
        return Err(error);
    }
    if let Some(previous_hash) = previous.avatar_hash {
        profile.remove_avatar_file(&previous_hash);
    }
    Ok(local_device_identity(&identity, &profile))
}

#[tauri::command]
fn network_snapshot(network: tauri::State<'_, NetworkNode>) -> NetworkSnapshot {
    network.snapshot()
}

#[tauri::command]
async fn current_connectivity(
    app: tauri::AppHandle,
    network: tauri::State<'_, NetworkNode>,
) -> Result<apple_connectivity::AppleConnectivity, String> {
    let connectivity = match apple_connectivity::snapshot(app).await {
        Ok(connectivity) => connectivity,
        Err(error) => {
            network
                .set_active_network(None)
                .map_err(|storage_error| storage_error.to_string())?;
            return Err(error);
        }
    };
    network
        .set_active_network(connectivity.active_network())
        .map_err(|error| error.to_string())?;
    Ok(connectivity)
}

#[tauri::command]
fn request_network_access(app: tauri::AppHandle) -> Result<(), String> {
    apple_connectivity::request_access(&app)
}

#[tauri::command]
fn trust_peer(
    network: tauri::State<'_, NetworkNode>,
    peer_id: String,
) -> Result<NetworkSnapshot, String> {
    network
        .set_peer_trust(&peer_id, TrustState::Trusted)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn block_peer(
    network: tauri::State<'_, NetworkNode>,
    peer_id: String,
) -> Result<NetworkSnapshot, String> {
    network
        .set_peer_trust(&peer_id, TrustState::Blocked)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn mark_peer_read(
    network: tauri::State<'_, NetworkNode>,
    peer_id: String,
    network_id: String,
) -> Result<NetworkSnapshot, String> {
    network
        .mark_peer_read(&peer_id, &network_id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn load_older_messages(
    network: tauri::State<'_, NetworkNode>,
    network_id: String,
    peer_id: String,
    before_created_at_unix_ms: u64,
    before_message_id: String,
    limit: usize,
) -> Result<HistoryPage, String> {
    network
        .load_older_messages(
            &network_id,
            &peer_id,
            before_created_at_unix_ms,
            &before_message_id,
            limit,
        )
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn storage_summary(network: tauri::State<'_, NetworkNode>) -> Result<StorageSummary, String> {
    network.storage_summary().map_err(|error| error.to_string())
}

#[tauri::command]
async fn clear_received_files(
    network: tauri::State<'_, NetworkNode>,
) -> Result<StorageSummary, String> {
    let network = network.inner().clone();
    tauri::async_runtime::spawn_blocking(move || network.clear_received_files())
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn connect_endpoint(
    network: tauri::State<'_, NetworkNode>,
    endpoint: String,
) -> Result<NetworkSnapshot, String> {
    let network = network.inner().clone();
    tauri::async_runtime::spawn_blocking(move || network.connect_endpoint(&endpoint))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn sync_peer_avatar(
    network: tauri::State<'_, NetworkNode>,
    peer_id: String,
) -> Result<NetworkSnapshot, String> {
    let network = network.inner().clone();
    tauri::async_runtime::spawn_blocking(move || network.sync_peer_avatar(&peer_id))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn send_text(
    network: tauri::State<'_, NetworkNode>,
    peer_id: String,
    text: String,
) -> Result<ChatMessage, String> {
    let network = network.inner().clone();
    tauri::async_runtime::spawn_blocking(move || network.send_text(&peer_id, &text))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn send_attachment(
    network: tauri::State<'_, NetworkNode>,
    peer_id: String,
    path: tauri_plugin_fs::FilePath,
    kind: AttachmentKind,
    file_name: Option<String>,
) -> Result<ChatMessage, String> {
    let source_path = path.into_path().map_err(|error| error.to_string())?;
    let network = network.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        network.send_attachment(&peer_id, &source_path, kind, file_name.as_deref())
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
fn cancel_message(
    network: tauri::State<'_, NetworkNode>,
    message_id: String,
) -> Result<NetworkSnapshot, String> {
    network
        .cancel_message(&message_id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn delete_message(
    network: tauri::State<'_, NetworkNode>,
    message_id: String,
) -> Result<NetworkSnapshot, String> {
    let network = network.inner().clone();
    tauri::async_runtime::spawn_blocking(move || network.delete_message(&message_id))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn retry_message(
    network: tauri::State<'_, NetworkNode>,
    message_id: String,
) -> Result<ChatMessage, String> {
    let network = network.inner().clone();
    tauri::async_runtime::spawn_blocking(move || network.retry_message(&message_id))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let app_data_dir = app.path().app_data_dir()?;
            let identity_path = app_data_dir.join(IDENTITY_FILE_NAME);
            #[cfg(any(target_os = "ios", target_os = "macos"))]
            let identity = Arc::new(DeviceIdentity::load_or_create_in_keychain(
                identity_path,
                IDENTITY_KEYCHAIN_SERVICE,
                IDENTITY_KEYCHAIN_ACCOUNT,
            )?);
            #[cfg(not(any(target_os = "ios", target_os = "macos")))]
            let identity = Arc::new(DeviceIdentity::load_or_create(identity_path)?);
            let profile = device_profile::DeviceProfileState::load_or_create(
                app_data_dir.join(device_profile::DEVICE_PROFILE_FILE_NAME),
                &identity.summary().display_id,
            )
            .map_err(std::io::Error::other)?;
            let store = Store::open(app_data_dir.join(DATABASE_FILE_NAME))?;
            let network = NetworkNode::start(
                Arc::clone(&identity),
                profile.snapshot().nickname,
                app_data_dir.join("attachments"),
                store,
            )?;
            if let Some(avatar_path) = profile.snapshot().avatar_path {
                network.set_avatar(Some(std::path::Path::new(&avatar_path)))?;
            }
            app.manage(identity);
            app.manage(profile);
            app.manage(network);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_status,
            device_identity,
            set_device_nickname,
            set_device_avatar,
            remove_device_avatar,
            network_snapshot,
            current_connectivity,
            request_network_access,
            trust_peer,
            block_peer,
            mark_peer_read,
            load_older_messages,
            storage_summary,
            clear_received_files,
            connect_endpoint,
            sync_peer_avatar,
            send_text,
            send_attachment,
            cancel_message,
            delete_message,
            retry_message
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|_app_handle, _event| {
        #[cfg(target_os = "ios")]
        if matches!(
            _event,
            tauri::RunEvent::WindowEvent {
                event: tauri::WindowEvent::Resumed,
                ..
            }
        ) {
            if let Some(network) = _app_handle.try_state::<NetworkNode>() {
                if let Err(error) = network.refresh_discovery() {
                    eprintln!("failed to refresh Bonjour registration: {error}");
                }
            }
        }
    });
}
