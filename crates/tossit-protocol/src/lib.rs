use serde::{Deserialize, Serialize};
use std::fmt;

pub const PROTOCOL_VERSION: u16 = 4;
pub const MAX_TEXT_BYTES: usize = 64 * 1024;
pub const MAX_ATTACHMENT_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_FILE_NAME_BYTES: usize = 255;
pub const MAX_MEDIA_TYPE_BYTES: usize = 127;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Envelope {
    pub protocol_version: u16,
    pub message_id: String,
    pub network_id: String,
    pub conversation_id: String,
    pub sender_peer_id: String,
    pub sender_sequence: u64,
    pub created_at_unix_ms: u64,
    pub payload: Payload,
}

impl Envelope {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(ValidationError::UnsupportedProtocolVersion {
                received: self.protocol_version,
            });
        }

        validate_identifier("messageId", &self.message_id)?;
        validate_identifier("networkId", &self.network_id)?;
        validate_identifier("conversationId", &self.conversation_id)?;
        validate_identifier("senderPeerId", &self.sender_peer_id)?;

        match &self.payload {
            Payload::Text { text } if text.is_empty() => Err(ValidationError::EmptyText),
            Payload::Text { text } if text.len() > MAX_TEXT_BYTES => {
                Err(ValidationError::TextTooLarge { bytes: text.len() })
            }
            Payload::Acknowledgement {
                acknowledged_message_id,
            } => validate_identifier("acknowledgedMessageId", acknowledged_message_id),
            Payload::Attachment {
                transfer_id,
                file_name,
                media_type,
                byte_size,
                ..
            } => {
                validate_identifier("transferId", transfer_id)?;
                validate_file_name(file_name)?;
                if media_type.is_empty()
                    || media_type.len() > MAX_MEDIA_TYPE_BYTES
                    || !media_type.is_ascii()
                    || media_type.chars().any(char::is_control)
                {
                    return Err(ValidationError::InvalidMediaType);
                }
                if *byte_size > MAX_ATTACHMENT_BYTES {
                    return Err(ValidationError::AttachmentTooLarge { bytes: *byte_size });
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Payload {
    Text {
        text: String,
    },
    Acknowledgement {
        #[serde(rename = "acknowledgedMessageId")]
        acknowledged_message_id: String,
    },
    Attachment {
        #[serde(rename = "transferId")]
        transfer_id: String,
        kind: AttachmentKind,
        #[serde(rename = "fileName")]
        file_name: String,
        #[serde(rename = "mediaType")]
        media_type: String,
        #[serde(rename = "byteSize")]
        byte_size: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AttachmentKind {
    Image,
    File,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValidationError {
    UnsupportedProtocolVersion { received: u16 },
    EmptyIdentifier { field: &'static str },
    EmptyText,
    TextTooLarge { bytes: usize },
    InvalidFileName,
    InvalidMediaType,
    AttachmentTooLarge { bytes: u64 },
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedProtocolVersion { received } => write!(
                formatter,
                "unsupported protocol version {received}; expected {PROTOCOL_VERSION}"
            ),
            Self::EmptyIdentifier { field } => write!(formatter, "{field} must not be empty"),
            Self::EmptyText => formatter.write_str("text payload must not be empty"),
            Self::TextTooLarge { bytes } => write!(
                formatter,
                "text payload is {bytes} bytes; maximum is {MAX_TEXT_BYTES}"
            ),
            Self::InvalidFileName => formatter.write_str("attachment file name is invalid"),
            Self::InvalidMediaType => formatter.write_str("attachment media type is invalid"),
            Self::AttachmentTooLarge { bytes } => write!(
                formatter,
                "attachment is {bytes} bytes; maximum is {MAX_ATTACHMENT_BYTES}"
            ),
        }
    }
}

impl std::error::Error for ValidationError {}

fn validate_identifier(field: &'static str, value: &str) -> Result<(), ValidationError> {
    if value.trim().is_empty() {
        return Err(ValidationError::EmptyIdentifier { field });
    }

    Ok(())
}

fn validate_file_name(value: &str) -> Result<(), ValidationError> {
    let windows_stem = value
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let reserved_windows_name = matches!(windows_stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (windows_stem.len() == 4
            && (windows_stem.starts_with("COM") || windows_stem.starts_with("LPT"))
            && matches!(windows_stem.as_bytes()[3], b'1'..=b'9'));
    if value.is_empty()
        || value.len() > MAX_FILE_NAME_BYTES
        || matches!(value, "." | "..")
        || value.ends_with(['.', ' '])
        || reserved_windows_name
        || value.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*'
                )
        })
    {
        return Err(ValidationError::InvalidFileName);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_envelope(text: impl Into<String>) -> Envelope {
        Envelope {
            protocol_version: PROTOCOL_VERSION,
            message_id: "message-1".into(),
            network_id: "network-1".into(),
            conversation_id: "conversation-1".into(),
            sender_peer_id: "peer-1".into(),
            sender_sequence: 1,
            created_at_unix_ms: 1_786_397_400_000,
            payload: Payload::Text { text: text.into() },
        }
    }

    #[test]
    fn text_envelope_round_trips_through_json() {
        let envelope = text_envelope("hello over LAN");

        let encoded = serde_json::to_string(&envelope).expect("serialize envelope");
        let decoded: Envelope = serde_json::from_str(&encoded).expect("deserialize envelope");

        assert_eq!(decoded, envelope);
        assert!(decoded.validate().is_ok());
    }

    #[test]
    fn future_protocol_version_is_rejected() {
        let envelope = Envelope {
            protocol_version: PROTOCOL_VERSION + 1,
            ..text_envelope("hello")
        };

        assert_eq!(
            envelope.validate(),
            Err(ValidationError::UnsupportedProtocolVersion {
                received: PROTOCOL_VERSION + 1
            })
        );
    }

    #[test]
    fn oversized_text_is_rejected_before_transport() {
        let envelope = text_envelope("x".repeat(MAX_TEXT_BYTES + 1));

        assert_eq!(
            envelope.validate(),
            Err(ValidationError::TextTooLarge {
                bytes: MAX_TEXT_BYTES + 1
            })
        );
    }

    #[test]
    fn attachment_metadata_round_trips_and_validates() {
        let envelope = Envelope {
            payload: Payload::Attachment {
                transfer_id: "transfer-1".into(),
                kind: AttachmentKind::Image,
                file_name: "photo.jpg".into(),
                media_type: "image/jpeg".into(),
                byte_size: 42,
            },
            ..text_envelope("replaced")
        };

        let encoded = serde_json::to_string(&envelope).expect("serialize attachment");
        let decoded: Envelope = serde_json::from_str(&encoded).expect("deserialize attachment");

        assert_eq!(decoded, envelope);
        assert!(decoded.validate().is_ok());
    }

    #[test]
    fn unsafe_or_oversized_attachment_is_rejected() {
        let unsafe_name = Envelope {
            payload: Payload::Attachment {
                transfer_id: "transfer-1".into(),
                kind: AttachmentKind::File,
                file_name: "../secret.txt".into(),
                media_type: "text/plain".into(),
                byte_size: 42,
            },
            ..text_envelope("replaced")
        };
        let oversized = Envelope {
            payload: Payload::Attachment {
                transfer_id: "transfer-2".into(),
                kind: AttachmentKind::File,
                file_name: "archive.zip".into(),
                media_type: "application/zip".into(),
                byte_size: MAX_ATTACHMENT_BYTES + 1,
            },
            ..text_envelope("replaced")
        };

        assert_eq!(
            unsafe_name.validate(),
            Err(ValidationError::InvalidFileName)
        );
        for file_name in ["CON.txt", "report?.pdf", "trailing. "] {
            let invalid = Envelope {
                payload: Payload::Attachment {
                    transfer_id: "transfer-invalid".into(),
                    kind: AttachmentKind::File,
                    file_name: file_name.into(),
                    media_type: "application/octet-stream".into(),
                    byte_size: 1,
                },
                ..text_envelope("replaced")
            };
            assert_eq!(invalid.validate(), Err(ValidationError::InvalidFileName));
        }
        assert_eq!(
            oversized.validate(),
            Err(ValidationError::AttachmentTooLarge {
                bytes: MAX_ATTACHMENT_BYTES + 1
            })
        );
    }
}
