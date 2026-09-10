//! Logical→physical address mapping.
//!
//! Every tree node and file extent in Btrfs is addressed by a *logical*
//! address. Chunks map ranges of that space onto physical device offsets. The
//! superblock carries a bootstrap array covering the chunk tree itself; the
//! rest come from the chunk tree.

use super::*;
use crate::device::{le_u16, le_u64};

/// Chunk type bits (`BTRFS_BLOCK_GROUP_*`).
pub const BLOCK_GROUP_DATA: u64 = 1 << 0;
pub const BLOCK_GROUP_SYSTEM: u64 = 1 << 1;
pub const BLOCK_GROUP_METADATA: u64 = 1 << 2;
pub const BLOCK_GROUP_RAID0: u64 = 1 << 3;
pub const BLOCK_GROUP_RAID1: u64 = 1 << 4;
pub const BLOCK_GROUP_DUP: u64 = 1 << 5;
pub const BLOCK_GROUP_RAID10: u64 = 1 << 6;
pub const BLOCK_GROUP_RAID5: u64 = 1 << 7;
pub const BLOCK_GROUP_RAID6: u64 = 1 << 8;
pub const BLOCK_GROUP_RAID1C3: u64 = 1 << 9;
pub const BLOCK_GROUP_RAID1C4: u64 = 1 << 10;

/// Profiles this reader can resolve from a single device.
///
/// Single, DUP, RAID1 and its C3/C4 variants all place a *complete* copy on
/// each device, so stripe 0 is always sufficient. Striped and parity profiles
/// need every member present and are refused rather than silently misread.
const SINGLE_DEVICE_READABLE: u64 =
    BLOCK_GROUP_RAID1 | BLOCK_GROUP_DUP | BLOCK_GROUP_RAID1C3 | BLOCK_GROUP_RAID1C4;
const STRIPED_OR_PARITY: u64 = BLOCK_GROUP_RAID0
    | BLOCK_GROUP_RAID10
    | BLOCK_GROUP_RAID5
    | BLOCK_GROUP_RAID6;

/// One chunk: a logical range and where its first copy lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub logical: u64,
    pub length: u64,
    pub chunk_type: u64,
    /// Physical offset of stripe 0 within its device.
    pub physical: u64,
    pub devid: u64,
    pub num_stripes: u16,
}

impl Chunk {
    pub fn contains(&self, logical: u64) -> bool {
        logical >= self.logical && logical < self.logical + self.length
    }

    /// Whether a single device is enough to read this chunk.
    pub fn readable_from_one_device(&self) -> bool {
        let profile = self.chunk_type & (SINGLE_DEVICE_READABLE | STRIPED_OR_PARITY);
        if profile & STRIPED_OR_PARITY != 0 {
            return false;
        }
        true // single (no profile bit) or a mirrored profile
    }
}

/// The set of known chunks, searched by logical address.
#[derive(Debug, Default, Clone)]
pub struct ChunkMap {
    chunks: Vec<Chunk>,
}

impl ChunkMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    pub fn insert(&mut self, c: Chunk) {
        // Later definitions win, which matters when the chunk tree restates a
        // chunk the bootstrap array already described.
        if let Some(existing) = self.chunks.iter_mut().find(|e| e.logical == c.logical) {
            *existing = c;
            return;
        }
        self.chunks.push(c);
        self.chunks.sort_by_key(|c| c.logical);
    }

    pub fn chunks(&self) -> &[Chunk] {
        &self.chunks
    }

    /// Translate a logical address, returning the physical offset and how many
    /// bytes remain contiguous within the chunk.
    pub fn resolve(&self, logical: u64) -> Result<(u64, u64), BtrfsError> {
        let c = self
            .chunks
            .iter()
            .find(|c| c.contains(logical))
            .ok_or(BtrfsError::Unmapped(logical))?;
        if !c.readable_from_one_device() {
            return Err(BtrfsError::UnsupportedProfile(c.chunk_type));
        }
        let into = logical - c.logical;
        Ok((c.physical + into, c.length - into))
    }

    /// Parse the superblock's bootstrap chunk array.
    ///
    /// The array is a sequence of `btrfs_disk_key` followed by `btrfs_chunk`,
    /// covering just enough of the address space to reach the chunk tree.
    pub fn from_sys_array(bytes: &[u8]) -> Result<Self, BtrfsError> {
        let mut map = ChunkMap::new();
        let mut at = 0usize;
        // disk_key is 17 bytes packed: objectid(8) + type(1) + offset(8).
        const KEY_LEN: usize = 17;
        while at + KEY_LEN <= bytes.len() {
            let key_type = bytes[at + 8];
            let logical = le_u64(bytes, at + 9);
            at += KEY_LEN;
            if key_type != CHUNK_ITEM_KEY {
                return Err(BtrfsError::Unsupported(format!(
                    "unexpected key type {key_type} in system chunk array"
                )));
            }
            let (chunk, used) = parse_chunk(bytes, at, logical)?;
            map.insert(chunk);
            at += used;
        }
        Ok(map)
    }
}

/// Parse a `btrfs_chunk` at `at`, returning it and its encoded length.
pub fn parse_chunk(b: &[u8], at: usize, logical: u64) -> Result<(Chunk, usize), BtrfsError> {
    // length(8) owner(8) stripe_len(8) type(8) io_align(4) io_width(4)
    // sector_size(4) num_stripes(2) sub_stripes(2) = 48 bytes, then stripes.
    const HEAD: usize = 48;
    const STRIPE: usize = 32; // devid(8) offset(8) dev_uuid(16)
    if at + HEAD > b.len() {
        return Err(BtrfsError::MalformedNode(logical, "chunk header truncated".into()));
    }
    let length = le_u64(b, at);
    let chunk_type = le_u64(b, at + 24);
    let num_stripes = le_u16(b, at + 44);
    if num_stripes == 0 {
        return Err(BtrfsError::MalformedNode(logical, "chunk has no stripes".into()));
    }
    let need = HEAD + num_stripes as usize * STRIPE;
    if at + need > b.len() {
        return Err(BtrfsError::MalformedNode(logical, "chunk stripes truncated".into()));
    }
    // Stripe 0 is a complete copy for every profile this reader accepts.
    let devid = le_u64(b, at + HEAD);
    let physical = le_u64(b, at + HEAD + 8);

    Ok((
        Chunk { logical, length, chunk_type, physical, devid, num_stripes },
        need,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk_bytes(length: u64, ty: u64, physical: u64, stripes: u16) -> Vec<u8> {
        let mut b = vec![0u8; 48 + stripes as usize * 32];
        b[0..8].copy_from_slice(&length.to_le_bytes());
        b[24..32].copy_from_slice(&ty.to_le_bytes());
        b[44..46].copy_from_slice(&stripes.to_le_bytes());
        b[48..56].copy_from_slice(&1u64.to_le_bytes()); // devid
        b[56..64].copy_from_slice(&physical.to_le_bytes());
        b
    }

    fn sys_array(logical: u64, chunk: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; 17];
        out[8] = CHUNK_ITEM_KEY;
        out[9..17].copy_from_slice(&logical.to_le_bytes());
        out.extend_from_slice(chunk);
        out
    }

    #[test]
    fn resolves_a_logical_address_inside_a_chunk() {
        let bytes = sys_array(1 << 20, &chunk_bytes(1 << 24, BLOCK_GROUP_SYSTEM, 4 << 20, 1));
        let map = ChunkMap::from_sys_array(&bytes).unwrap();
        assert_eq!(map.len(), 1);

        // Start of the chunk maps to the stripe's physical offset.
        assert_eq!(map.resolve(1 << 20).unwrap(), (4 << 20, 1 << 24));
        // An address inside keeps its remainder.
        let (phys, run) = map.resolve((1 << 20) + 4096).unwrap();
        assert_eq!(phys, (4 << 20) + 4096);
        assert_eq!(run, (1 << 24) - 4096);
    }

    #[test]
    fn an_address_outside_every_chunk_is_an_error() {
        let bytes = sys_array(1 << 20, &chunk_bytes(1 << 24, BLOCK_GROUP_SYSTEM, 4 << 20, 1));
        let map = ChunkMap::from_sys_array(&bytes).unwrap();
        assert!(matches!(map.resolve(1), Err(BtrfsError::Unmapped(1))));
        // Just past the end is also unmapped, not clamped.
        assert!(map.resolve((1 << 20) + (1 << 24)).is_err());
    }

    #[test]
    fn mirrored_profiles_are_readable_from_one_disk() {
        for profile in [BLOCK_GROUP_RAID1, BLOCK_GROUP_DUP, BLOCK_GROUP_RAID1C3, BLOCK_GROUP_RAID1C4] {
            let bytes = sys_array(0, &chunk_bytes(1 << 20, BLOCK_GROUP_DATA | profile, 999, 2));
            let map = ChunkMap::from_sys_array(&bytes).unwrap();
            assert_eq!(map.resolve(0).unwrap().0, 999, "profile 0x{profile:x}");
        }
    }

    #[test]
    fn striped_and_parity_profiles_are_refused_not_misread() {
        for profile in [BLOCK_GROUP_RAID0, BLOCK_GROUP_RAID10, BLOCK_GROUP_RAID5, BLOCK_GROUP_RAID6] {
            let bytes = sys_array(0, &chunk_bytes(1 << 20, BLOCK_GROUP_DATA | profile, 999, 4));
            let map = ChunkMap::from_sys_array(&bytes).unwrap();
            assert!(
                matches!(map.resolve(0), Err(BtrfsError::UnsupportedProfile(_))),
                "profile 0x{profile:x} must be refused"
            );
        }
    }

    #[test]
    fn a_later_definition_replaces_the_bootstrap_copy() {
        let mut map = ChunkMap::new();
        map.insert(Chunk { logical: 0, length: 100, chunk_type: 0, physical: 10, devid: 1, num_stripes: 1 });
        map.insert(Chunk { logical: 0, length: 100, chunk_type: 0, physical: 20, devid: 1, num_stripes: 1 });
        assert_eq!(map.len(), 1, "same logical start must not duplicate");
        assert_eq!(map.resolve(0).unwrap().0, 20);
    }

    #[test]
    fn a_truncated_chunk_is_rejected() {
        let mut bytes = sys_array(0, &chunk_bytes(1 << 20, BLOCK_GROUP_SYSTEM, 0, 4));
        bytes.truncate(bytes.len() - 40); // lose part of the stripe array
        assert!(ChunkMap::from_sys_array(&bytes).is_err());
    }
}
