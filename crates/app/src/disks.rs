//! macOS disk discovery (SPEC §7).
//!
//! This module runs exactly one external command, `diskutil list -plist`, which
//! reads the partition map and nothing else. There is no code path here that
//! mounts, unmounts, ejects, or writes: discovery must be safe to run while the
//! user is doing something else with their disks.

use serde::{Deserialize, Serialize};
use std::process::Command;

/// A whole disk as macOS reports it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MacDisk {
    /// BSD name, e.g. `disk4`. Never used alone as identity (SPEC §3).
    pub bsd_name: String,
    pub media_name: String,
    pub size_bytes: u64,
    pub internal: bool,
    pub protocol: Option<String>,
    pub partitions: Vec<MacPartition>,
    /// What this disk looks like it is, from the partition map alone.
    pub role: DiskRole,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MacPartition {
    pub bsd_name: String,
    pub content: String,
    pub size_bytes: u64,
    pub volume_name: Option<String>,
    pub mount_point: Option<String>,
}

/// Classification used to steer the UI. Never a permission: the user still
/// confirms the source explicitly, and the worker re-verifies independently.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiskRole {
    /// Carries a Linux RAID partition — a plausible Synology source.
    SynologyCandidate,
    /// Mounted and writable: a plausible destination.
    DestinationCandidate,
    /// The disk macOS booted from. Never offered as either.
    SystemDisk,
    Other,
}

/// Partition content type Synology's data partitions carry.
const LINUX_RAID_CONTENT: &str = "Linux_RAID";

#[derive(Debug, thiserror::Error)]
pub enum DiskError {
    #[error("could not run diskutil: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("diskutil exited with status {0}")]
    Status(String),
    #[error("could not parse diskutil output: {0}")]
    Parse(String),
}

/// List every whole disk attached to this Mac.
pub fn list_disks() -> Result<Vec<MacDisk>, DiskError> {
    let out = Command::new("/usr/sbin/diskutil").args(["list", "-plist"]).output()?;
    if !out.status.success() {
        return Err(DiskError::Status(out.status.to_string()));
    }
    let plist = String::from_utf8_lossy(&out.stdout);
    let names = parse_whole_disk_names(&plist);

    let mut disks = Vec::new();
    for name in names {
        if let Ok(d) = describe_disk(&name) {
            disks.push(d);
        }
    }
    Ok(disks)
}

fn describe_disk(bsd_name: &str) -> Result<MacDisk, DiskError> {
    let info = disk_info(bsd_name)?;
    let partitions = list_partitions(bsd_name)?;
    // diskutil spells this `Device Location: Internal|External`; older
    // versions also emit `Internal: Yes|No`. Accept either.
    let internal = info
        .get("Device Location")
        .map(|v| v.eq_ignore_ascii_case("Internal"))
        .or_else(|| info.get("Internal").map(|v| v == "Yes"))
        .unwrap_or(false);
    let size_bytes = info
        .get("Disk Size")
        .and_then(|v| extract_bytes(v))
        .unwrap_or(0);

    let role = classify(internal, &partitions);
    Ok(MacDisk {
        bsd_name: bsd_name.to_string(),
        media_name: info.get("Device / Media Name").cloned().unwrap_or_default(),
        size_bytes,
        internal,
        protocol: info.get("Protocol").cloned(),
        partitions,
        role,
    })
}

fn disk_info(bsd_name: &str) -> Result<std::collections::HashMap<String, String>, DiskError> {
    let out = Command::new("/usr/sbin/diskutil")
        .args(["info", bsd_name])
        .output()?;
    if !out.status.success() {
        return Err(DiskError::Status(out.status.to_string()));
    }
    Ok(parse_info(&String::from_utf8_lossy(&out.stdout)))
}

fn list_partitions(bsd_name: &str) -> Result<Vec<MacPartition>, DiskError> {
    let out = Command::new("/usr/sbin/diskutil")
        .args(["list", bsd_name])
        .output()?;
    if !out.status.success() {
        return Err(DiskError::Status(out.status.to_string()));
    }
    let mut parts = parse_partition_rows(&String::from_utf8_lossy(&out.stdout));
    // `diskutil list` does not report mount points, and classification needs
    // them to tell a usable destination from an unmounted disk.
    for p in &mut parts {
        if let Ok(info) = disk_info(&p.bsd_name) {
            p.mount_point = info
                .get("Mount Point")
                .filter(|v| !v.is_empty() && v.as_str() != "Not applicable (no file system)")
                .cloned();
            p.volume_name = info.get("Volume Name").cloned();
            p.size_bytes = info
                .get("Disk Size")
                .or_else(|| info.get("Volume Total Space"))
                .and_then(|v| extract_bytes(v))
                .unwrap_or(0);
        }
    }
    Ok(parts)
}

/// Decide what a disk is for, from its partition map.
pub fn classify(internal: bool, partitions: &[MacPartition]) -> DiskRole {
    if partitions.iter().any(|p| p.content == LINUX_RAID_CONTENT) {
        return DiskRole::SynologyCandidate;
    }
    if internal {
        return DiskRole::SystemDisk;
    }
    if partitions.iter().any(|p| p.mount_point.is_some()) {
        return DiskRole::DestinationCandidate;
    }
    DiskRole::Other
}

/// Whether `dest` may receive files recovered from `source` (SPEC §3).
///
/// A destination on the same physical disk as the source would mean writing to
/// the source, so it is refused regardless of how it is mounted.
pub fn destination_is_safe(source: &MacDisk, dest: &MacDisk) -> bool {
    source.bsd_name != dest.bsd_name && dest.role != DiskRole::SynologyCandidate
}

// ---- parsing helpers, kept pure so they can be tested without any disk ----

fn parse_whole_disk_names(plist: &str) -> Vec<String> {
    // WholeDisks is a <array><string>diskN</string>… block.
    let Some(start) = plist.find("<key>WholeDisks</key>") else {
        return Vec::new();
    };
    let rest = &plist[start..];
    let Some(end) = rest.find("</array>") else { return Vec::new() };
    rest[..end]
        .match_indices("<string>")
        .filter_map(|(i, _)| {
            let s = &rest[i + "<string>".len()..];
            s.find("</string>").map(|e| s[..e].to_string())
        })
        .collect()
}

fn parse_info(text: &str) -> std::collections::HashMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

/// Pull the byte count out of e.g. `10.0 TB (10000831348224 Bytes) (...)`.
fn extract_bytes(s: &str) -> Option<u64> {
    let start = s.find('(')? + 1;
    let rest = &s[start..];
    let end = rest.find(" Bytes")?;
    rest[..end].replace(',', "").trim().parse().ok()
}

/// Parse the table `diskutil list <disk>` prints.
fn parse_partition_rows(text: &str) -> Vec<MacPartition> {
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        // Rows look like: "   2:   Microsoft Basic Data Expansion   10.0 TB   disk4s2"
        let Some((idx, rest)) = t.split_once(':') else { continue };
        if idx.trim().parse::<u32>().is_err() {
            continue;
        }
        let Some(bsd) = rest.split_whitespace().last() else { continue };
        // A partition node is `diskNsM`. The container row ends in plain
        // `diskN`, and "disk" itself contains an 's', so check the suffix
        // after the disk number rather than the whole string.
        if !is_partition_node(bsd) {
            continue;
        }
        let content = rest.split_whitespace().next().unwrap_or_default().to_string();
        out.push(MacPartition {
            bsd_name: bsd.to_string(),
            content,
            size_bytes: 0,
            volume_name: None,
            mount_point: None,
        });
    }
    out
}

/// True for `diskNsM` (a partition), false for `diskN` (a whole disk).
fn is_partition_node(bsd: &str) -> bool {
    let Some(tail) = bsd.strip_prefix("disk") else { return false };
    let Some((num, part)) = tail.split_once('s') else { return false };
    !num.is_empty()
        && num.bytes().all(|b| b.is_ascii_digit())
        && !part.is_empty()
        && part.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(content: &str, mount: Option<&str>) -> MacPartition {
        MacPartition {
            bsd_name: "disk9s1".into(),
            content: content.into(),
            size_bytes: 0,
            volume_name: None,
            mount_point: mount.map(str::to_string),
        }
    }

    fn disk(name: &str, role: DiskRole) -> MacDisk {
        MacDisk {
            bsd_name: name.into(),
            media_name: "test".into(),
            size_bytes: 0,
            internal: false,
            protocol: None,
            partitions: vec![],
            role,
        }
    }

    #[test]
    fn a_linux_raid_partition_marks_a_synology_candidate() {
        let parts = vec![part("EFI", None), part("Linux_RAID", None)];
        assert_eq!(classify(false, &parts), DiskRole::SynologyCandidate);
    }

    #[test]
    fn an_exfat_usb_disk_is_a_destination_not_a_source() {
        let parts = vec![part("Microsoft", Some("/Volumes/Expansion"))];
        assert_eq!(classify(false, &parts), DiskRole::DestinationCandidate);
    }

    #[test]
    fn the_boot_disk_is_never_offered() {
        let parts = vec![part("Apple_APFS", Some("/"))];
        assert_eq!(classify(true, &parts), DiskRole::SystemDisk);
    }

    /// A Synology disk that macOS happened to mount must still read as a
    /// source, never as a destination.
    #[test]
    fn a_mounted_synology_disk_is_still_a_source() {
        let parts = vec![part("Linux_RAID", Some("/Volumes/whatever"))];
        assert_eq!(classify(false, &parts), DiskRole::SynologyCandidate);
    }

    #[test]
    fn the_source_disk_can_never_be_its_own_destination() {
        let src = disk("disk4", DiskRole::SynologyCandidate);
        assert!(!destination_is_safe(&src, &src));
    }

    #[test]
    fn another_synology_disk_is_not_a_valid_destination() {
        let src = disk("disk4", DiskRole::SynologyCandidate);
        let other = disk("disk5", DiskRole::SynologyCandidate);
        assert!(!destination_is_safe(&src, &other));
    }

    #[test]
    fn a_separate_external_disk_is_a_valid_destination() {
        let src = disk("disk4", DiskRole::SynologyCandidate);
        let dest = disk("disk6", DiskRole::DestinationCandidate);
        assert!(destination_is_safe(&src, &dest));
    }

    #[test]
    fn internal_is_read_from_device_location() {
        let text = "   Device / Media Name:      APPLE SSD\n   Device Location:           Internal\n";
        let info = parse_info(text);
        assert_eq!(info.get("Device Location").map(String::as_str), Some("Internal"));

        let ext = parse_info("   Device Location:           External\n");
        assert_eq!(ext.get("Device Location").map(String::as_str), Some("External"));
    }

    #[test]
    fn byte_counts_survive_diskutils_formatting() {
        assert_eq!(
            extract_bytes("10.0 TB (10000831348224 Bytes) (exactly 19532873727 512-Byte-Units)"),
            Some(10000831348224)
        );
        assert_eq!(extract_bytes("no parenthesised size here"), None);
    }

    #[test]
    fn whole_disk_names_are_read_from_the_plist() {
        let plist = r#"
          <key>WholeDisks</key>
          <array><string>disk0</string><string>disk4</string></array>
          <key>AllDisks</key><array><string>disk0s1</string></array>"#;
        assert_eq!(parse_whole_disk_names(plist), vec!["disk0", "disk4"]);
    }

    #[test]
    fn a_whole_disk_node_is_not_mistaken_for_a_partition() {
        // "disk" contains an 's', so a naive substring check would accept these.
        assert!(!is_partition_node("disk4"));
        assert!(!is_partition_node("disk"));
        assert!(is_partition_node("disk4s2"));
        assert!(is_partition_node("disk12s34"));
    }

    #[test]
    fn partition_rows_are_parsed_from_the_list_table() {
        // Real shape of `diskutil list disk4` output.
        let text = "\
/dev/disk4 (external, physical):
   #:                       TYPE NAME                    SIZE       IDENTIFIER
   0:      GUID_partition_scheme                        *10.0 TB    disk4
   1:                        EFI EFI                     209.7 MB   disk4s1
   2:       Microsoft Basic Data Expansion               10.0 TB    disk4s2
";
        let parts = parse_partition_rows(text);
        // Row 0 is the container itself and has no sN suffix, so it is skipped.
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].bsd_name, "disk4s1");
        assert_eq!(parts[1].bsd_name, "disk4s2");
    }
}
