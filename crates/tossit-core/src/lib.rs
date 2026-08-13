use serde::Serialize;
use tossit_protocol::PROTOCOL_VERSION;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationStatus {
    pub product_name: &'static str,
    pub app_version: &'static str,
    pub protocol_version: u16,
    pub implementation_phase: &'static str,
    pub capabilities: CapabilityStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityStatus {
    pub persistent_identity: bool,
    pub discovery: bool,
    pub manual_endpoint_connection: bool,
    pub encrypted_messaging: bool,
    pub trusted_peers: bool,
    pub durable_history: bool,
}

pub fn application_status() -> ApplicationStatus {
    ApplicationStatus {
        product_name: "TossIt",
        app_version: env!("CARGO_PKG_VERSION"),
        protocol_version: PROTOCOL_VERSION,
        implementation_phase: "phase-2",
        capabilities: CapabilityStatus {
            persistent_identity: true,
            discovery: true,
            manual_endpoint_connection: true,
            encrypted_messaging: true,
            trusted_peers: true,
            durable_history: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_reports_only_implemented_capabilities() {
        let status = application_status();

        assert_eq!(status.product_name, "TossIt");
        assert_eq!(status.protocol_version, PROTOCOL_VERSION);
        assert_eq!(status.implementation_phase, "phase-2");
        assert!(status.capabilities.persistent_identity);
        assert!(status.capabilities.discovery);
        assert!(status.capabilities.manual_endpoint_connection);
        assert!(status.capabilities.encrypted_messaging);
        assert!(status.capabilities.trusted_peers);
        assert!(status.capabilities.durable_history);
    }
}
