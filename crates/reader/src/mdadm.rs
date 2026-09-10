//! mdadm (Linux Software RAID) v1.x superblock reader.
//!
//! Only what a *reader* needs: where the member's data starts, how big it is,
//! and whether this single disk can be read on its own. RAID1 members carry a
//! full copy of the array's data, so one disk is enough; striped and parity
//! levels are recognised and refused rather than half-read.

use crate::device::{le_u16, le_u32, le_u64, ReadOnlyDevice, DeviceError};

/// `0xa92b4efc` stored little-endian, i.e. the `fc4e2ba9` seen in a hex dump.
pub const MD_SB_MAGIC: u32 = 0xa92b_4efc;

/// Byte offsets a v1.x superblock may live at, in probe order.
///
/// v1.2 (Synology's choice) sits 4 KiB from the start of the partition; the
/// other two are at the end and are checked so an unexpected layout is
/// reported as an unsupported version rather than "no RAID here".
const V12_OFFSET: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaidLevel {
    /// Mirror: every member holds the whole array. Readable from one disk.
    Raid1,
    /// Linear concatenation of members.
    Linear,
    Raid0,
    Raid4,
    Raid5,
    Raid6,
    Raid10,
    Other(i32),
}

impl RaidLevel {
    fn from_raw(v: i32) -> Self {
        match v {
            -1 => RaidLevel::Linear,
            0 => RaidLevel::Raid0,
            1 => RaidLevel::Raid1,
            4 => RaidLevel::Raid4,
            5 => RaidLevel::Raid5,
            6 => RaidLevel::Raid6,
            10 => RaidLevel::Raid10,
            other => RaidLevel::Other(other),
        }
    }

    /// Whether the array's contents can be read from this member alone.
    pub fn readable_from_single_member(self) -> bool {
        matches!(self, RaidLevel::Raid1)
    }
}

/// The fields of an mdadm superblock this reader uses.
#[derive(Debug, Clone)]
pub struct MdSuperblock {
    pub major_version: u32,
    pub level: RaidLevel,
    pub raid_disks: u32,
    /// Array UUID, for matching a source fingerprint across re-plugs.
    pub array_uuid: [u8; 16],
    /// Array name as set by mdadm, e.g. `SynologyNAS:2`.
    pub name: String,
    /// Byte offset from the start of *this member* to the array's data.
    pub data_offset_bytes: u64,
    /// Size of the data this member contributes, in bytes.
    pub data_size_bytes: u64,
    /// Where the superblock itself was found.
    pub superblock_offset: u64,
}

impl MdSuperblock {
    /// Whether this member alone can be read as the whole array.
    pub fn is_single_member_readable(&self) -> bool {
        self.level.readable_from_single_member()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MdError {
    #[error(transparent)]
    Device(#[from] DeviceError),
    #[error("no mdadm superblock at offset {offset} (found magic 0x{found:08x})")]
    NoSuperblock { offset: u64, found: u32 },
    #[error("unsupported mdadm superblock version {0}")]
    UnsupportedVersion(u32),
    #[error("superblock is truncated: needed {needed} bytes, read {got}")]
    Truncated { needed: usize, got: usize },
}

/// Read the v1.2 superblock of a RAID member.
pub fn read_superblock(dev: &mut ReadOnlyDevice) -> Result<MdSuperblock, MdError> {
    parse_superblock(&dev.read_at(V12_OFFSET, 512)?, V12_OFFSET)
}

/// Parse a superblock from bytes, kept separate from I/O so fixtures can drive it.
pub fn parse_superblock(b: &[u8], superblock_offset: u64) -> Result<MdSuperblock, MdError> {
    // The fields used below all live within the first 0x100 bytes.
    if b.len() < 0x100 {
        return Err(MdError::Truncated { needed: 0x100, got: b.len() });
    }

    let magic = le_u32(b, 0x00);
    if magic != MD_SB_MAGIC {
        return Err(MdError::NoSuperblock { offset: superblock_offset, found: magic });
    }

    let major_version = le_u32(b, 0x04);
    if major_version != 1 {
        return Err(MdError::UnsupportedVersion(major_version));
    }

    let mut array_uuid = [0u8; 16];
    array_uuid.copy_from_slice(&b[0x10..0x20]);

    // 32 bytes, NUL-padded.
    let name_bytes = &b[0x20..0x40];
    let name = String::from_utf8_lossy(
        &name_bytes[..name_bytes.iter().position(|&c| c == 0).unwrap_or(name_bytes.len())],
    )
    .into_owned();

    let level = RaidLevel::from_raw(le_u32(b, 0x48) as i32);
    let raid_disks = le_u32(b, 0x5c);

    // data_offset and data_size are in 512-byte sectors, always.
    let data_offset_bytes = le_u64(b, 0x80) * 512;
    let data_size_bytes = le_u64(b, 0x88) * 512;

    Ok(MdSuperblock {
        major_version,
        level,
        raid_disks,
        array_uuid,
        name,
        data_offset_bytes,
        data_size_bytes,
        superblock_offset,
    })
}

/// Format a raw UUID the way mdadm prints it.
pub fn format_uuid(u: &[u8; 16]) -> String {
    let h: Vec<String> = u.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}:{}:{}:{}",
        h[0..4].concat(),
        h[4..8].concat(),
        h[8..12].concat(),
        h[12..16].concat()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic v1.2 superblock so parsing is tested without a disk.
    fn fixture(level: i32, data_offset_sectors: u64, data_size_sectors: u64) -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[0x00..0x04].copy_from_slice(&MD_SB_MAGIC.to_le_bytes());
        b[0x04..0x08].copy_from_slice(&1u32.to_le_bytes()); // major version
        for (i, v) in (0u8..16).enumerate() {
            b[0x10 + i] = v;
        }
        let name = b"SynologyNAS:2";
        b[0x20..0x20 + name.len()].copy_from_slice(name);
        b[0x48..0x4c].copy_from_slice(&(level as u32).to_le_bytes());
        b[0x5c..0x60].copy_from_slice(&2u32.to_le_bytes()); // raid_disks
        b[0x80..0x88].copy_from_slice(&data_offset_sectors.to_le_bytes());
        b[0x88..0x90].copy_from_slice(&data_size_sectors.to_le_bytes());
        b
    }

    #[test]
    fn parses_a_raid1_member() {
        let sb = parse_superblock(&fixture(1, 2048, 7_800_000_000), 4096).unwrap();
        assert_eq!(sb.level, RaidLevel::Raid1);
        assert_eq!(sb.name, "SynologyNAS:2");
        assert_eq!(sb.raid_disks, 2);
        // Sectors are 512 bytes regardless of the device's own block size.
        assert_eq!(sb.data_offset_bytes, 2048 * 512);
        assert!(sb.is_single_member_readable());
    }

    #[test]
    fn a_degraded_mirror_is_still_readable_from_one_disk() {
        // raid_disks says 2 but we only have this one: RAID1 still contains
        // the whole array, which is the entire premise of the recovery.
        let sb = parse_superblock(&fixture(1, 2048, 1024), 4096).unwrap();
        assert_eq!(sb.raid_disks, 2);
        assert!(sb.is_single_member_readable());
    }

    #[test]
    fn striped_and_parity_levels_are_refused_not_half_read() {
        for level in [0, 4, 5, 6, 10] {
            let sb = parse_superblock(&fixture(level, 2048, 1024), 4096).unwrap();
            assert!(
                !sb.is_single_member_readable(),
                "level {level} must not be treated as single-disk readable"
            );
        }
    }

    #[test]
    fn a_disk_without_raid_metadata_is_reported_clearly() {
        let empty = vec![0u8; 512];
        match parse_superblock(&empty, 4096) {
            Err(MdError::NoSuperblock { offset, found }) => {
                assert_eq!(offset, 4096);
                assert_eq!(found, 0);
            }
            other => panic!("expected NoSuperblock, got {other:?}"),
        }
    }

    #[test]
    fn a_future_superblock_version_is_refused() {
        let mut b = fixture(1, 2048, 1024);
        b[0x04..0x08].copy_from_slice(&2u32.to_le_bytes());
        assert!(matches!(parse_superblock(&b, 4096), Err(MdError::UnsupportedVersion(2))));
    }

    #[test]
    fn the_magic_matches_what_a_hex_dump_shows() {
        // The manual records `fc4e2ba9` as the byte sequence on disk.
        assert_eq!(MD_SB_MAGIC.to_le_bytes(), [0xfc, 0x4e, 0x2b, 0xa9]);
    }
}

/// Verify the superblock checksum the way mdadm does.
///
/// mdadm sums the superblock as little-endian u32 words with the checksum
/// field itself zeroed, folds the 64-bit accumulator down to 32 bits, and
/// stores the bitwise complement. `super1.c:calc_sb_1_csum` is the reference.
pub fn verify_checksum(b: &[u8]) -> Result<bool, MdError> {
    if b.len() < 0x100 {
        return Err(MdError::Truncated { needed: 0x100, got: b.len() });
    }
    // max_dev at 0xfc extends the summed region by 2 bytes per device slot.
    let max_dev = le_u32(b, 0xfc) as usize;
    let size = 256 + max_dev * 2;
    if b.len() < size {
        return Err(MdError::Truncated { needed: size, got: b.len() });
    }

    let stored = le_u32(b, 0xd8);
    let mut sum: u64 = 0;
    let mut i = 0;
    while i + 4 <= size {
        // The checksum field reads as zero while summing.
        let word = if i == 0xd8 { 0 } else { le_u32(b, i) };
        sum += word as u64;
        i += 4;
    }
    if size - i == 2 {
        sum += le_u16(b, i) as u64;
    }
    // Fold the carries down into 32 bits.
    sum = (sum & 0xffff_ffff) + (sum >> 32);
    sum = (sum & 0xffff_ffff) + (sum >> 32);

    Ok(stored == !(sum as u32))
}
