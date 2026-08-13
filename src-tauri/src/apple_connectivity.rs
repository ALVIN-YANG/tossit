use serde::Serialize;
use tossit_network::ActiveNetwork;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ConnectivityKind {
    Wifi,
    LocalNetwork,
    Cellular,
    Offline,
    #[cfg(not(any(target_os = "ios", target_os = "macos")))]
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum NetworkPermission {
    Prompt,
    Granted,
    Limited,
    Denied,
    Restricted,
    #[cfg(not(any(target_os = "ios", target_os = "macos")))]
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppleConnectivity {
    pub kind: ConnectivityKind,
    pub permission: NetworkPermission,
    pub ssid: Option<String>,
    pub network_id: Option<String>,
    pub can_message: bool,
}

impl AppleConnectivity {
    pub fn active_network(&self) -> Option<ActiveNetwork> {
        Some(ActiveNetwork {
            network_id: self.network_id.clone()?,
            display_name: self.ssid.clone()?,
        })
    }
}

#[cfg(any(target_os = "ios", target_os = "macos"))]
mod apple {
    use super::{AppleConnectivity, ConnectivityKind, NetworkPermission};
    use if_addrs::get_if_addrs;
    use objc2_core_location::{CLAuthorizationStatus, CLLocationManager};
    use sha2::{Digest, Sha256};
    use std::net::IpAddr;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tauri::{AppHandle, Runtime};

    static LOCATION_MANAGER_RETAINED: AtomicBool = AtomicBool::new(false);

    pub async fn snapshot<R: Runtime>(app: AppHandle<R>) -> Result<AppleConnectivity, String> {
        let permission = location_permission();
        let ssid = current_ssid(app).await?;
        let (has_local_route, has_cellular_route) = interface_routes();
        let kind = if ssid.is_some() {
            ConnectivityKind::Wifi
        } else if has_local_route {
            ConnectivityKind::LocalNetwork
        } else if has_cellular_route {
            ConnectivityKind::Cellular
        } else {
            ConnectivityKind::Offline
        };
        let permission = match (permission, ssid.is_some()) {
            (NetworkPermission::Granted, false) if kind == ConnectivityKind::LocalNetwork => {
                NetworkPermission::Limited
            }
            (permission, _) => permission,
        };
        let network_id = ssid.as_deref().map(network_id_for_ssid);
        Ok(AppleConnectivity {
            kind,
            permission,
            can_message: network_id.is_some(),
            ssid,
            network_id,
        })
    }

    pub fn request_access<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
        if location_permission() != NetworkPermission::Prompt {
            return Ok(());
        }
        app.run_on_main_thread(|| unsafe {
            let manager = CLLocationManager::new();
            manager.requestWhenInUseAuthorization();
            if !LOCATION_MANAGER_RETAINED.swap(true, Ordering::Relaxed) {
                std::mem::forget(manager);
            }
        })
        .map_err(|error| error.to_string())
    }

    #[allow(deprecated)]
    fn location_permission() -> NetworkPermission {
        let status = unsafe { CLLocationManager::authorizationStatus_class() };
        match status {
            CLAuthorizationStatus::NotDetermined => NetworkPermission::Prompt,
            CLAuthorizationStatus::Restricted => NetworkPermission::Restricted,
            CLAuthorizationStatus::Denied => NetworkPermission::Denied,
            CLAuthorizationStatus::AuthorizedAlways
            | CLAuthorizationStatus::AuthorizedWhenInUse => NetworkPermission::Granted,
            _ => NetworkPermission::Prompt,
        }
    }

    fn interface_routes() -> (bool, bool) {
        let mut has_local_route = false;
        let mut has_cellular_route = false;
        for interface in get_if_addrs().unwrap_or_default() {
            let ip = interface.ip();
            if ip.is_loopback() || ip.is_unspecified() {
                continue;
            }
            if interface.name.starts_with("pdp_ip") {
                has_cellular_route = true;
                continue;
            }
            if match ip {
                IpAddr::V4(ip) => ip.is_private() || ip.is_link_local(),
                IpAddr::V6(ip) => ip.is_unique_local() || ip.is_unicast_link_local(),
            } {
                has_local_route = true;
            }
        }
        (has_local_route, has_cellular_route)
    }

    fn network_id_for_ssid(ssid: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"tossit-network-space-v1\0");
        hasher.update(ssid.as_bytes());
        hex::encode_upper(hasher.finalize())
    }

    #[cfg(target_os = "macos")]
    async fn current_ssid<R: Runtime>(_app: AppHandle<R>) -> Result<Option<String>, String> {
        use objc2_core_wlan::CWWiFiClient;

        let ssid = unsafe {
            CWWiFiClient::sharedWiFiClient()
                .interface()
                .and_then(|interface| interface.ssid())
                .map(|ssid| ssid.to_string())
        };
        Ok(ssid.filter(|ssid| !ssid.is_empty()))
    }

    #[cfg(target_os = "ios")]
    async fn current_ssid<R: Runtime>(app: AppHandle<R>) -> Result<Option<String>, String> {
        use block2::RcBlock;
        use objc2_network_extension::NEHotspotNetwork;
        use std::ptr::NonNull;
        use std::sync::mpsc;
        use std::time::Duration;

        let (sender, receiver) = mpsc::sync_channel(1);
        app.run_on_main_thread(move || unsafe {
            let completion = RcBlock::new(move |network: *mut NEHotspotNetwork| {
                let ssid = NonNull::new(network).map(|network| {
                    // SAFETY: NetworkExtension owns the object for the duration of this callback.
                    network.as_ref().SSID().to_string()
                });
                let _ = sender.send(ssid);
            });
            NEHotspotNetwork::fetchCurrentWithCompletionHandler(&completion);
        })
        .map_err(|error| error.to_string())?;
        tauri::async_runtime::spawn_blocking(move || {
            receiver
                .recv_timeout(Duration::from_secs(3))
                .map(|ssid| ssid.filter(|ssid| !ssid.is_empty()))
                .map_err(|_| "读取当前 Wi-Fi 超时".to_owned())
        })
        .await
        .map_err(|error| error.to_string())?
    }

    #[cfg(test)]
    mod tests {
        use super::network_id_for_ssid;

        #[test]
        fn network_id_is_stable_and_scoped_to_ssid() {
            assert_eq!(network_id_for_ssid("Home"), network_id_for_ssid("Home"));
            assert_ne!(network_id_for_ssid("Home"), network_id_for_ssid("Office"));
            assert_eq!(network_id_for_ssid("Home").len(), 64);
        }
    }
}

#[cfg(any(target_os = "ios", target_os = "macos"))]
pub use apple::{request_access, snapshot};

#[cfg(not(any(target_os = "ios", target_os = "macos")))]
pub async fn snapshot<R: tauri::Runtime>(
    _app: tauri::AppHandle<R>,
) -> Result<AppleConnectivity, String> {
    Ok(AppleConnectivity {
        kind: ConnectivityKind::Unsupported,
        permission: NetworkPermission::Unsupported,
        ssid: None,
        network_id: None,
        can_message: false,
    })
}

#[cfg(not(any(target_os = "ios", target_os = "macos")))]
pub fn request_access<R: tauri::Runtime>(_app: &tauri::AppHandle<R>) -> Result<(), String> {
    Ok(())
}
