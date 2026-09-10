//! Btrfs reader — enough of the format to list and extract files.
//!
//! Structure layouts follow btrfs-progs' `kernel-shared/uapi/btrfs_tree.h`.
//! Only reading is implemented: no allocator, no transaction, no repair path,
//! which is why this stays a tractable amount of code.
//!
//! Reading a file takes four steps:
//!   1. superblock → tree roots and the bootstrap chunk array
//!   2. chunk tree → logical→physical address mapping
//!   3. root tree  → locate each subvolume's filesystem tree
//!   4. fs tree    → directory entries, inodes, and extents

pub mod chunk;
pub mod fs;
pub mod superblock;
pub mod tree;

pub use chunk::ChunkMap;
pub use fs::{DirEntry, EntryType, Extent, FsReader, Inode, Subvolume};
pub use superblock::Superblock;

/// `BTRFS_MAGIC` — the ASCII bytes `_BHRfS_M`.
pub const BTRFS_MAGIC: u64 = 0x4D5F_5366_5248_425F;
/// Primary superblock offset. Backups live at 64 MiB, 256 GiB and 1 PiB.
pub const SUPER_INFO_OFFSET: u64 = 65536;
pub const SUPER_INFO_SIZE: usize = 4096;
pub const CSUM_SIZE: usize = 32;
pub const FSID_SIZE: usize = 16;
pub const UUID_SIZE: usize = 16;
pub const SYSTEM_CHUNK_ARRAY_SIZE: usize = 2048;
pub const LABEL_SIZE: usize = 256;

/// Backup superblock offsets, tried in order when the primary is unusable.
pub const SUPER_MIRRORS: [u64; 3] = [67_108_864, 274_877_906_944, 1_125_899_906_842_624];

/// Well-known object ids (`BTRFS_*_OBJECTID`).
pub const ROOT_TREE_OBJECTID: u64 = 1;
pub const CHUNK_TREE_OBJECTID: u64 = 3;
pub const FS_TREE_OBJECTID: u64 = 5;
pub const FIRST_FREE_OBJECTID: u64 = 256;

/// Item types (`BTRFS_*_KEY`).
pub const INODE_ITEM_KEY: u8 = 1;
pub const INODE_REF_KEY: u8 = 12;
pub const DIR_ITEM_KEY: u8 = 84;
pub const DIR_INDEX_KEY: u8 = 96;
pub const EXTENT_DATA_KEY: u8 = 108;
pub const ROOT_ITEM_KEY: u8 = 132;
pub const ROOT_REF_KEY: u8 = 156;
pub const CHUNK_ITEM_KEY: u8 = 228;

#[derive(Debug, thiserror::Error)]
pub enum BtrfsError {
    #[error(transparent)]
    Device(#[from] crate::device::DeviceError),
    #[error("no btrfs superblock: magic was 0x{0:016x}")]
    BadMagic(u64),
    #[error("logical address {0} is not mapped by any chunk")]
    Unmapped(u64),
    #[error("unsupported chunk profile 0x{0:x}: this reader handles single, DUP and RAID1")]
    UnsupportedProfile(u64),
    #[error("tree node at {0} is malformed: {1}")]
    MalformedNode(u64, String),
    #[error("{0}")]
    Unsupported(String),
}
