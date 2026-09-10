//! Wire types for the macOS controller <-> Linux worker protocol (SPEC §12).
//!
//! Newline-delimited JSON. The controller sends [`Command`], the worker
//! replies with [`Event`]. There is deliberately no field carrying a shell
//! string: every command names an operation the worker already knows how to
//! perform, so a compromised UI cannot ask the worker to run something new.

use serde::{Deserialize, Serialize};

pub mod flags;

/// Requests from the macOS controller to the worker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    ScanDisks {
        request_id: String,
    },
    Preflight {
        request_id: String,
        source_id: String,
    },
    Inventory {
        request_id: String,
        root_id: u64,
        path: String,
        depth: u32,
    },
    StartBatch {
        request_id: String,
        batch_path: String,
        mode: BatchMode,
    },
    Status {
        request_id: String,
        run_id: String,
    },
    Stop {
        request_id: String,
        run_id: String,
    },
    Verify {
        request_id: String,
        scope: String,
        mode: VerifyMode,
    },
    Cleanup {
        request_id: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BatchMode {
    /// Restore every path in the batch.
    Full,
    /// Restore only paths the destination index reports as missing.
    MissingOnly,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerifyMode {
    /// SPEC §10 Mode A — compare path sets.
    Inventory,
    /// SPEC §10 Mode B — compare logical sizes.
    Size,
    /// SPEC §10 Mode C — compare content hashes.
    Hash,
}

/// Messages from the worker back to the controller.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Emitted only after `blockdev --getro` was observed to return 1.
    SourceReadOnlyConfirmed {
        device: String,
        read_only: bool,
    },
    PreflightComplete {
        source: SourceIdentity,
        data_stack: Vec<String>,
        roots: Vec<Root>,
        warnings: Vec<String>,
    },
    InventoryProgress {
        entries: u64,
    },
    BatchStarted {
        path: String,
    },
    DestinationBytes {
        bytes: u64,
    },
    Warning {
        code: WarningCode,
        path: Option<String>,
        message: String,
    },
    Error {
        code: ErrorCode,
        path: Option<String>,
        message: String,
    },
    BatchComplete {
        path: String,
    },
}

/// Stable identity of a source disk (SPEC §3). A BSD/Linux device name alone
/// is never sufficient: names are reassigned across reboots and re-plugs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SourceIdentity {
    pub device: String,
    pub serial: String,
    pub capacity_bytes: u64,
    pub read_only: bool,
    pub raid_uuid: Option<String>,
    pub btrfs_uuid: Option<String>,
    pub label: Option<String>,
}

impl SourceIdentity {
    /// Whether this is the same physical disk as `other`.
    ///
    /// Compares only attributes that travel with the disk. `device` is
    /// excluded on purpose, so re-plugging a disk as a different node still
    /// matches, and a *different* disk appearing at a remembered node does not.
    pub fn matches(&self, other: &Self) -> bool {
        self.serial == other.serial
            && self.capacity_bytes == other.capacity_bytes
            && self.raid_uuid == other.raid_uuid
            && self.btrfs_uuid == other.btrfs_uuid
    }
}

/// A Btrfs subvolume root, as listed by `btrfs restore -l`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Root {
    pub id: u64,
    pub name: Option<String>,
    #[serde(default)]
    pub selected: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WarningCode {
    FilenameUnsupported,
    FilenameLengthExceeded,
    UnicodeNormalizationDiffers,
    EaDirSkipped,
}

/// SPEC §14. Every variant means "stop and preserve state" unless the table
/// in the spec says otherwise.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    SourceNotReadOnly,
    SourceFingerprintChanged,
    MdAssemblyFailed,
    LvmActivationFailed,
    UnknownRootFlag,
    BtrfsReadError,
    DestinationDisconnected,
    DestinationLowSpace,
    FilenameUnsupported,
    WorkerCrashed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_roundtrips_through_json() {
        let cmd = Command::StartBatch {
            request_id: "r1".into(),
            batch_path: "Medias/Movie".into(),
            mode: BatchMode::MissingOnly,
        };
        let line = serde_json::to_string(&cmd).unwrap();
        assert_eq!(cmd, serde_json::from_str(&line).unwrap());
    }

    #[test]
    fn command_wire_format_matches_spec() {
        let line = serde_json::to_string(&Command::Stop {
            request_id: "r9".into(),
            run_id: "run-1".into(),
        })
        .unwrap();
        assert!(line.contains(r#""command":"stop""#), "{line}");
    }

    #[test]
    fn identity_ignores_device_node_but_not_serial() {
        let a = SourceIdentity {
            device: "/dev/sda".into(),
            serial: "SN123".into(),
            capacity_bytes: 8_000_000_000_000,
            ..Default::default()
        };
        // Same disk, replugged as a different node.
        let replugged = SourceIdentity { device: "/dev/sdb".into(), ..a.clone() };
        assert!(a.matches(&replugged));

        // A different disk that happens to occupy the remembered node.
        let impostor = SourceIdentity { serial: "SN999".into(), ..a.clone() };
        assert!(!a.matches(&impostor));
    }
}
