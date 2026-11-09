use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const fn bluetooth_uuid(short: u16) -> Uuid {
    bluetooth_uuid32(short as u32)
}

pub const fn bluetooth_uuid32(short: u32) -> Uuid {
    Uuid::from_u128(((short as u128) << 96) | 0x0000_1000_8000_0080_5f9b_34fb)
}

pub const FTMS_SERVICE: Uuid = bluetooth_uuid(0x1826);
pub const TREADMILL_DATA: Uuid = bluetooth_uuid(0x2acd);
pub const KINGSMITH_SERVICE: Uuid = bluetooth_uuid(0x1234);
pub const WILINK_SERVICE: Uuid = bluetooth_uuid(0xfe00);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceSummary {
    /// Opaque OS identifier, not assumed to be a MAC address.
    pub id: String,
    pub name: Option<String>,
    pub rssi: Option<i16>,
    pub advertised_services: Vec<Uuid>,
}

impl DeviceSummary {
    pub fn is_treadmill_candidate(&self) -> bool {
        let name = self
            .name
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        name.contains("walkingpad")
            || name.contains("kingsmith")
            || name.starts_with("ks-")
            || self.advertised_services.iter().any(|id| {
                [FTMS_SERVICE, WILINK_SERVICE].contains(id)
                    || crate::kingsmith::channels(*id).is_some()
            })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CharacteristicProperties {
    pub read: bool,
    pub write: bool,
    pub write_without_response: bool,
    pub notify: bool,
    pub indicate: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CharacteristicInfo {
    pub uuid: Uuid,
    pub properties: CharacteristicProperties,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceInfo {
    pub uuid: Uuid,
    pub primary: bool,
    pub characteristics: Vec<CharacteristicInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolFamily {
    Ftms,
    Kingsmith1234,
    Wilink,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolCandidate {
    pub family: ProtocolFamily,
    /// A GATT match does not prove the device's packet format or compatibility.
    pub evidence: String,
    pub decoder_available: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceProfile {
    pub schema_version: u8,
    pub device: DeviceSummary,
    pub services: Vec<ServiceInfo>,
    pub protocol_candidates: Vec<ProtocolCandidate>,
}

impl DeviceProfile {
    pub fn new(device: DeviceSummary, services: Vec<ServiceInfo>) -> Self {
        let mut protocol_candidates = Vec::new();
        for service in &services {
            if service.uuid == FTMS_SERVICE {
                let has_data = service.characteristics.iter().any(|c| {
                    c.uuid == TREADMILL_DATA && (c.properties.notify || c.properties.indicate)
                });
                protocol_candidates.push(ProtocolCandidate {
                    family: ProtocolFamily::Ftms,
                    evidence: if has_data {
                        "FTMS service and subscribable Treadmill Data characteristic"
                    } else {
                        "FTMS service only; no subscribable Treadmill Data characteristic"
                    }
                    .into(),
                    decoder_available: has_data,
                });
            } else if let Some((write, notify)) = crate::kingsmith::channels(service.uuid) {
                let has_channels = service
                    .characteristics
                    .iter()
                    .any(|c| c.uuid == notify && c.properties.notify)
                    && service
                        .characteristics
                        .iter()
                        .any(|c| c.uuid == write && c.properties.write_without_response);
                protocol_candidates.push(ProtocolCandidate {
                    family: ProtocolFamily::Kingsmith1234,
                    evidence:
                        "KingSmith text-protocol GATT alias; handshake selects a supported alphabet"
                            .into(),
                    decoder_available: has_channels,
                });
            } else if service.uuid == WILINK_SERVICE {
                protocol_candidates.push(ProtocolCandidate {
                    family: if service.uuid == KINGSMITH_SERVICE {
                        ProtocolFamily::Kingsmith1234
                    } else {
                        ProtocolFamily::Wilink
                    },
                    evidence: "Vendor service detected; packet format and handshake unverified"
                        .into(),
                    decoder_available: false,
                });
            }
        }
        Self {
            schema_version: 1,
            device,
            services,
            protocol_candidates,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_uuid_expansion() {
        assert_eq!(
            FTMS_SERVICE.to_string(),
            "00001826-0000-1000-8000-00805f9b34fb"
        );
    }

    #[test]
    fn captured_x21_32_bit_alias_selects_the_text_driver() {
        let captured: DeviceProfile =
            serde_json::from_str(include_str!("../../../fixtures/x21-gatt-profile.json")).unwrap();
        let profile = DeviceProfile::new(captured.device, captured.services);
        assert_eq!(profile.protocol_candidates.len(), 1);
        assert_eq!(
            profile.protocol_candidates[0].family,
            ProtocolFamily::Kingsmith1234
        );
        assert!(profile.protocol_candidates[0].decoder_available);
        assert_ne!(profile.services[0].uuid, KINGSMITH_SERVICE);
    }

    #[test]
    fn model_name_does_not_select_protocol() {
        let device = DeviceSummary {
            id: "test".into(),
            name: Some("KS-X21".into()),
            rssi: None,
            advertised_services: vec![],
        };
        assert!(device.is_treadmill_candidate());
        assert!(
            DeviceProfile::new(device, vec![])
                .protocol_candidates
                .is_empty()
        );
    }

    #[test]
    fn ftms_without_treadmill_data_is_not_declared_decodable() {
        let device = DeviceSummary {
            id: "test".into(),
            name: None,
            rssi: None,
            advertised_services: vec![],
        };
        let profile = DeviceProfile::new(
            device,
            vec![ServiceInfo {
                uuid: FTMS_SERVICE,
                primary: true,
                characteristics: vec![],
            }],
        );
        assert!(!profile.protocol_candidates[0].decoder_available);
    }
}
