//! Btrfs superblock.
//!
//! Field order follows `struct btrfs_super_block`. The first four fields match
//! `struct btrfs_header`, so the offsets below are shared with tree nodes.

use super::*;
use crate::device::{le_u16, le_u32, le_u64, ReadOnlyDevice};

/// Offsets within the superblock, derived from the packed C layout.
mod off {
    use super::{CSUM_SIZE, FSID_SIZE};
    pub const CSUM: usize = 0;
    pub const FSID: usize = CSUM + CSUM_SIZE; // 32
    pub const BYTENR: usize = FSID + FSID_SIZE; // 48
    pub const FLAGS: usize = BYTENR + 8; // 56
    pub const MAGIC: usize = FLAGS + 8; // 64
    pub const GENERATION: usize = MAGIC + 8; // 72
    pub const ROOT: usize = GENERATION + 8; // 80
    pub const CHUNK_ROOT: usize = ROOT + 8; // 88
    pub const LOG_ROOT: usize = CHUNK_ROOT + 8; // 96
    pub const TOTAL_BYTES: usize = LOG_ROOT + 8 + 8; // 112 (skips unused transid)
    pub const BYTES_USED: usize = TOTAL_BYTES + 8; // 120
    pub const ROOT_DIR_OBJECTID: usize = BYTES_USED + 8; // 128
    pub const NUM_DEVICES: usize = ROOT_DIR_OBJECTID + 8; // 136
    pub const SECTORSIZE: usize = NUM_DEVICES + 8; // 144
    pub const NODESIZE: usize = SECTORSIZE + 4; // 148
    pub const STRIPESIZE: usize = NODESIZE + 4 + 4; // 156 (skips unused leafsize)
    pub const SYS_CHUNK_ARRAY_SIZE: usize = STRIPESIZE + 4; // 160
    pub const CHUNK_ROOT_GENERATION: usize = SYS_CHUNK_ARRAY_SIZE + 4; // 164
    pub const COMPAT_FLAGS: usize = CHUNK_ROOT_GENERATION + 8; // 172
    pub const COMPAT_RO_FLAGS: usize = COMPAT_FLAGS + 8; // 180
    pub const INCOMPAT_FLAGS: usize = COMPAT_RO_FLAGS + 8; // 188
    pub const CSUM_TYPE: usize = INCOMPAT_FLAGS + 8; // 196
    pub const ROOT_LEVEL: usize = CSUM_TYPE + 2; // 198
    pub const CHUNK_ROOT_LEVEL: usize = ROOT_LEVEL + 1; // 199
    pub const LOG_ROOT_LEVEL: usize = CHUNK_ROOT_LEVEL + 1; // 200
    /// `struct btrfs_dev_item` is 98 bytes; the label follows it.
    pub const DEV_ITEM: usize = LOG_ROOT_LEVEL + 1; // 201
    pub const LABEL: usize = DEV_ITEM + 98; // 299
    /// After the label come cache_generation(8), uuid_tree_generation(8),
    /// metadata_uuid(16), nr_global_roots(8), remap_root(8),
    /// remap_root_generation(8), remap_root_level(1) and reserved[199].
    pub const AFTER_LABEL: usize = 8 + 8 + 16 + 8 + 8 + 8 + 1 + 199; // 256
    pub const SYS_CHUNK_ARRAY: usize = LABEL + super::LABEL_SIZE + AFTER_LABEL; // 811
}

/// Checksum algorithms (`btrfs_csum_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CsumType {
    Crc32c,
    Xxhash64,
    Sha256,
    Blake2b,
    Unknown(u16),
}

impl CsumType {
    fn from_raw(v: u16) -> Self {
        match v {
            0 => CsumType::Crc32c,
            1 => CsumType::Xxhash64,
            2 => CsumType::Sha256,
            3 => CsumType::Blake2b,
            other => CsumType::Unknown(other),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Superblock {
    pub fsid: [u8; FSID_SIZE],
    /// Physical offset this copy was read from.
    pub bytenr: u64,
    pub generation: u64,
    /// Logical address of the root tree.
    pub root: u64,
    /// Logical address of the chunk tree.
    pub chunk_root: u64,
    pub log_root: u64,
    pub total_bytes: u64,
    pub bytes_used: u64,
    pub num_devices: u64,
    pub sectorsize: u32,
    pub nodesize: u32,
    pub root_level: u8,
    pub chunk_root_level: u8,
    pub csum_type: CsumType,
    pub incompat_flags: u64,
    pub label: String,
    /// Bootstrap chunk mapping, needed before the chunk tree can be read.
    pub sys_chunk_array: Vec<u8>,
}

impl Superblock {
    /// Read the primary superblock at `base + 65536`.
    ///
    /// `base` is where the Btrfs filesystem starts within the device — for the
    /// Synology stack that is the RAID data offset plus the LV's offset.
    pub fn read(dev: &mut ReadOnlyDevice, base: u64) -> Result<Self, BtrfsError> {
        let bytes = dev.read_at(base + SUPER_INFO_OFFSET, SUPER_INFO_SIZE)?;
        Self::parse(&bytes)
    }

    pub fn parse(b: &[u8]) -> Result<Self, BtrfsError> {
        if b.len() < SUPER_INFO_SIZE {
            return Err(BtrfsError::Unsupported(format!(
                "superblock read was {} bytes, need {SUPER_INFO_SIZE}",
                b.len()
            )));
        }
        let magic = le_u64(b, off::MAGIC);
        if magic != BTRFS_MAGIC {
            return Err(BtrfsError::BadMagic(magic));
        }

        let mut fsid = [0u8; FSID_SIZE];
        fsid.copy_from_slice(&b[off::FSID..off::FSID + FSID_SIZE]);

        let label_raw = &b[off::LABEL..off::LABEL + LABEL_SIZE];
        let label = String::from_utf8_lossy(
            &label_raw[..label_raw.iter().position(|&c| c == 0).unwrap_or(0)],
        )
        .into_owned();

        let sys_len = le_u32(b, off::SYS_CHUNK_ARRAY_SIZE) as usize;
        let sys_len = sys_len.min(SYSTEM_CHUNK_ARRAY_SIZE);
        let sys_chunk_array =
            b[off::SYS_CHUNK_ARRAY..off::SYS_CHUNK_ARRAY + sys_len].to_vec();

        Ok(Superblock {
            fsid,
            bytenr: le_u64(b, off::BYTENR),
            generation: le_u64(b, off::GENERATION),
            root: le_u64(b, off::ROOT),
            chunk_root: le_u64(b, off::CHUNK_ROOT),
            log_root: le_u64(b, off::LOG_ROOT),
            total_bytes: le_u64(b, off::TOTAL_BYTES),
            bytes_used: le_u64(b, off::BYTES_USED),
            num_devices: le_u64(b, off::NUM_DEVICES),
            sectorsize: le_u32(b, off::SECTORSIZE),
            nodesize: le_u32(b, off::NODESIZE),
            root_level: b[off::ROOT_LEVEL],
            chunk_root_level: b[off::CHUNK_ROOT_LEVEL],
            csum_type: CsumType::from_raw(le_u16(b, off::CSUM_TYPE)),
            incompat_flags: le_u64(b, off::INCOMPAT_FLAGS),
            label,
            sys_chunk_array,
        })
    }

    /// FSID formatted the way `blkid` prints it.
    pub fn fsid_string(&self) -> String {
        let h: Vec<String> = self.fsid.iter().map(|b| format!("{b:02x}")).collect();
        format!(
            "{}-{}-{}-{}-{}",
            h[0..4].concat(),
            h[4..6].concat(),
            h[6..8].concat(),
            h[8..10].concat(),
            h[10..16].concat()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<u8> {
        let mut b = vec![0u8; SUPER_INFO_SIZE];
        b[off::MAGIC..off::MAGIC + 8].copy_from_slice(&BTRFS_MAGIC.to_le_bytes());
        b[off::BYTENR..off::BYTENR + 8].copy_from_slice(&65536u64.to_le_bytes());
        b[off::GENERATION..off::GENERATION + 8].copy_from_slice(&42u64.to_le_bytes());
        b[off::ROOT..off::ROOT + 8].copy_from_slice(&30_000_000u64.to_le_bytes());
        b[off::CHUNK_ROOT..off::CHUNK_ROOT + 8].copy_from_slice(&20_000_000u64.to_le_bytes());
        b[off::TOTAL_BYTES..off::TOTAL_BYTES + 8]
            .copy_from_slice(&4_000_000_000_000u64.to_le_bytes());
        b[off::SECTORSIZE..off::SECTORSIZE + 4].copy_from_slice(&4096u32.to_le_bytes());
        b[off::NODESIZE..off::NODESIZE + 4].copy_from_slice(&16384u32.to_le_bytes());
        b[off::NUM_DEVICES..off::NUM_DEVICES + 8].copy_from_slice(&1u64.to_le_bytes());
        b[off::ROOT_LEVEL] = 1;
        b[off::CHUNK_ROOT_LEVEL] = 0;
        let label = b"volume1";
        b[off::LABEL..off::LABEL + label.len()].copy_from_slice(label);
        b[off::SYS_CHUNK_ARRAY_SIZE..off::SYS_CHUNK_ARRAY_SIZE + 4]
            .copy_from_slice(&97u32.to_le_bytes());
        b
    }

    #[test]
    fn the_magic_is_the_ascii_the_manual_records() {
        // The manual notes `_BHRfS_M` as the on-disk marker.
        assert_eq!(&BTRFS_MAGIC.to_le_bytes(), b"_BHRfS_M");
    }

    #[test]
    fn parses_the_fields_a_reader_needs() {
        let sb = Superblock::parse(&fixture()).unwrap();
        assert_eq!(sb.generation, 42);
        assert_eq!(sb.root, 30_000_000);
        assert_eq!(sb.chunk_root, 20_000_000);
        assert_eq!(sb.nodesize, 16384);
        assert_eq!(sb.sectorsize, 4096);
        assert_eq!(sb.label, "volume1");
        assert_eq!(sb.csum_type, CsumType::Crc32c);
        assert_eq!(sb.sys_chunk_array.len(), 97);
    }

    #[test]
    fn a_non_btrfs_region_is_rejected_rather_than_misparsed() {
        let zeros = vec![0u8; SUPER_INFO_SIZE];
        assert!(matches!(Superblock::parse(&zeros), Err(BtrfsError::BadMagic(0))));
    }

    #[test]
    fn a_short_read_is_an_error_not_a_panic() {
        let short = vec![0u8; 100];
        assert!(Superblock::parse(&short).is_err());
    }

    /// The offsets are derived by hand from the packed C struct, so pin the
    /// ones a mistake would silently corrupt.
    #[test]
    fn field_offsets_match_the_packed_c_layout() {
        assert_eq!(off::MAGIC, 64);
        assert_eq!(off::ROOT, 80);
        assert_eq!(off::CHUNK_ROOT, 88);
        assert_eq!(off::SECTORSIZE, 144);
        assert_eq!(off::NODESIZE, 148);
        assert_eq!(off::SYS_CHUNK_ARRAY_SIZE, 160);
        assert_eq!(off::INCOMPAT_FLAGS, 188);
        assert_eq!(off::DEV_ITEM, 201);
        assert_eq!(off::LABEL, 299);
        // btrfs-progs places sys_chunk_array at 811; an off-by-N here makes
        // the bootstrap chunk array parse as garbage.
        assert_eq!(off::SYS_CHUNK_ARRAY, 811);
        // Everything must still fit inside one superblock.
        assert!(off::SYS_CHUNK_ARRAY + SYSTEM_CHUNK_ARRAY_SIZE <= SUPER_INFO_SIZE);
    }

    #[test]
    fn the_system_chunk_array_cannot_exceed_its_bound() {
        let mut b = fixture();
        // A corrupt length must not cause an out-of-range slice.
        b[off::SYS_CHUNK_ARRAY_SIZE..off::SYS_CHUNK_ARRAY_SIZE + 4]
            .copy_from_slice(&u32::MAX.to_le_bytes());
        let sb = Superblock::parse(&b).unwrap();
        assert_eq!(sb.sys_chunk_array.len(), SYSTEM_CHUNK_ARRAY_SIZE);
    }
}
