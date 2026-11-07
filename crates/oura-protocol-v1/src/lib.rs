//! Stable version-one collector upload messages.
//!
//! Every wire message includes `protocol_version`. Existing field names and
//! meanings are fixed; future compatible additions must be optional.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Identity {
    Demo,
    Production,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UploadMetadata {
    pub protocol_version: u32,
    pub schema_version: i64,
    pub collector_id: String,
    pub collector_epoch: String,
    pub generation: u64,
    pub created_unix: i64,
    pub size_bytes: u64,
    pub sha256: String,
    pub build_revision: String,
    pub identity: Identity,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UploadAck {
    pub protocol_version: u32,
    pub accepted: bool,
    pub generation: u64,
    pub active_generation: u64,
    pub duplicate: bool,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ErrorEnvelope {
    pub protocol_version: u32,
    pub code: String,
    pub message: String,
}

/// A command the hub asks the collector to run. Frozen allowlist: new commands
/// are additive; nothing here is ever renamed or reinterpreted.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandName {
    SyncNow,
    RefreshDeviceInfo,
    RefreshBattery,
    CollectorStatus,
    StartLiveHr,
    StopLiveHr,
    InspectSupportedFeatures,
    SetFeatureMode,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReceiptState {
    Accepted,
    Rejected,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResultState {
    Completed,
    Failed,
    Cancelled,
    Expired,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamKind {
    LiveHr,
}

/// One live sample.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Sample {
    pub bpm: u32,
    pub unix: i64,
}

/// The collector's own view, as distinct flags. Ring availability is never
/// inferred from Pi connectivity.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CollectorState {
    pub pi_reachable: bool,
    pub adapter_present: bool,
    pub ring_recently_seen: bool,
    pub ring_connected: bool,
    pub operation_running: bool,
    pub ring_unreachable: bool,
    pub unknown: bool,
}

/// Control channel messages. The `type` tag discriminates; every variant carries
/// `protocol_version`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMessage {
    Hello {
        protocol_version: u32,
        collector_id: String,
        build_revision: String,
    },
    Command {
        protocol_version: u32,
        id: String,
        name: CommandName,
        #[serde(default)]
        args: serde_json::Value,
        created_unix: i64,
        expires_unix: i64,
    },
    Receipt {
        protocol_version: u32,
        id: String,
        state: ReceiptState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    // The frozen architecture frame is `type: "result"`, while the Rust variant
    // stays `CommandResult` so call sites read naturally.
    #[serde(rename = "result")]
    CommandResult {
        protocol_version: u32,
        id: String,
        state: ResultState,
        #[serde(default)]
        detail: serde_json::Value,
    },
    Status {
        protocol_version: u32,
        collector_state: CollectorState,
    },
    Stream {
        protocol_version: u32,
        id: String,
        kind: StreamKind,
        sample: Sample,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_uses_stable_lowercase_names() {
        assert_eq!(serde_json::to_string(&Identity::Demo).unwrap(), "\"demo\"");
        assert_eq!(
            serde_json::to_string(&Identity::Production).unwrap(),
            "\"production\""
        );
        assert_eq!(
            serde_json::from_str::<Identity>("\"production\"").unwrap(),
            Identity::Production
        );
    }

    #[test]
    fn metadata_round_trips_without_changing_opaque_epoch() {
        let metadata = UploadMetadata {
            protocol_version: PROTOCOL_VERSION,
            schema_version: 2,
            collector_id: "pi-01".into(),
            collector_epoch: "2026-11-07T09:00:00Z/1".into(),
            generation: 42,
            created_unix: 1_794_041_840,
            size_bytes: 7_340_032,
            sha256: "00".repeat(32),
            build_revision: "revision".into(),
            identity: Identity::Production,
        };
        let json = serde_json::to_string(&metadata).unwrap();
        assert_eq!(
            serde_json::from_str::<UploadMetadata>(&json).unwrap(),
            metadata
        );
    }

    #[test]
    fn replies_require_protocol_version() {
        let ack = r#"{"accepted":true,"generation":1,"active_generation":1,"duplicate":false,"message":"accepted"}"#;
        let error = r#"{"code":"unauthorized","message":"authentication required"}"#;
        assert!(serde_json::from_str::<UploadAck>(ack).is_err());
        assert!(serde_json::from_str::<ErrorEnvelope>(error).is_err());
    }

    #[test]
    fn control_result_frame_uses_the_frozen_result_tag() {
        // ARCHITECTURE.md freezes `type: "result"`. The Rust variant is
        // `CommandResult`, so this rename is load-bearing: without it the wire
        // tag becomes `command_result` and the collector drops the frame.
        let message = ControlMessage::CommandResult {
            protocol_version: PROTOCOL_VERSION,
            id: "01J".into(),
            state: ResultState::Completed,
            detail: serde_json::Value::Null,
        };
        let json = serde_json::to_string(&message).unwrap();
        assert!(
            json.contains(r#""type":"result""#),
            "wire tag must be `result`, got {json}"
        );
        assert!(!json.contains("command_result"), "got {json}");
        assert_eq!(
            serde_json::from_str::<ControlMessage>(&json).unwrap(),
            message
        );
    }

    #[test]
    fn control_frames_round_trip() {
        let command = ControlMessage::Command {
            protocol_version: PROTOCOL_VERSION,
            id: "01J".into(),
            name: CommandName::SyncNow,
            args: serde_json::Value::Null,
            created_unix: 1_794_041_840,
            expires_unix: 1_794_045_440,
        };
        let status = ControlMessage::Status {
            protocol_version: PROTOCOL_VERSION,
            collector_state: CollectorState {
                pi_reachable: true,
                ring_connected: true,
                ..Default::default()
            },
        };
        for message in [command, status] {
            let json = serde_json::to_string(&message).unwrap();
            assert_eq!(
                serde_json::from_str::<ControlMessage>(&json).unwrap(),
                message
            );
        }
    }

    #[test]
    fn control_messages_require_protocol_version() {
        let command =
            r#"{"type":"command","id":"01J","name":"sync_now","created_unix":0,"expires_unix":0}"#;
        let status = r#"{"type":"status","collector_state":{}}"#;
        assert!(serde_json::from_str::<ControlMessage>(command).is_err());
        assert!(serde_json::from_str::<ControlMessage>(status).is_err());
    }
}
