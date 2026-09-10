//! Filesystem tree: subvolumes, directories, inodes and file data.
//!
//! Structure layouts follow `btrfs_tree.h`. This is the layer the UI talks to:
//! list a directory, then read a file's bytes out of its extents.

use super::*;
use crate::device::{le_u16, le_u32, le_u64, ReadOnlyDevice};
use crate::btrfs::tree::TreeReader;

/// Entry types stored in a directory item (`BTRFS_FT_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryType {
    File,
    Dir,
    Symlink,
    Other(u8),
}

impl EntryType {
    fn from_raw(v: u8) -> Self {
        match v {
            1 => EntryType::File,
            2 => EntryType::Dir,
            7 => EntryType::Symlink,
            other => EntryType::Other(other),
        }
    }
}

/// One directory entry.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    /// Inode number of the target within this subvolume.
    pub inode: u64,
    pub entry_type: EntryType,
    /// Set when the entry points at another subvolume rather than an inode.
    pub subvolume: Option<u64>,
}

/// The inode fields a reader needs.
#[derive(Debug, Clone, Copy)]
pub struct Inode {
    pub size: u64,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub mtime_secs: i64,
}

impl Inode {
    pub fn is_dir(&self) -> bool {
        self.mode & 0o170000 == 0o040000
    }
    pub fn is_file(&self) -> bool {
        self.mode & 0o170000 == 0o100000
    }
    pub fn is_symlink(&self) -> bool {
        self.mode & 0o170000 == 0o120000
    }

    fn parse(d: &[u8]) -> Option<Self> {
        if d.len() < 160 {
            return None;
        }
        // Field offsets within the 160-byte struct: generation@0 transid@8
        // size@16 nbytes@24 block_group@32 nlink@40 uid@44 gid@48 mode@52
        // rdev@56 flags@64 sequence@72 reserved[4]@80 atime@112 ctime@124
        // mtime@136 otime@148. Each timespec is sec(8) + nsec(4).
        const MTIME: usize = 136;
        Some(Inode {
            size: le_u64(d, 16),
            nlink: le_u32(d, 40),
            uid: le_u32(d, 44),
            gid: le_u32(d, 48),
            mode: le_u32(d, 52),
            mtime_secs: le_u64(d, MTIME) as i64,
        })
    }
}

/// A subvolume as named in the root tree.
#[derive(Debug, Clone)]
pub struct Subvolume {
    pub id: u64,
    pub name: String,
    /// Logical address of this subvolume's tree root.
    pub root_bytenr: u64,
    /// Inode of the subvolume's own root directory.
    pub root_dirid: u64,
    /// Raw `flags` from the root item — Synology sets private bits here.
    pub flags: u64,
}

/// How a file's bytes are stored.
#[derive(Debug, Clone)]
pub enum Extent {
    /// Data stored directly inside the tree item.
    Inline { data: Vec<u8> },
    /// Data on disk at a logical address. `disk_bytenr == 0` means a hole.
    Regular {
        file_offset: u64,
        disk_bytenr: u64,
        /// Offset into the extent where this file's data begins.
        data_offset: u64,
        num_bytes: u64,
        compression: u8,
    },
    /// Allocated but never written: reads as zeroes.
    Hole { file_offset: u64, num_bytes: u64 },
}

/// Reads a Btrfs filesystem: subvolumes, directories and file contents.
pub struct FsReader<'a> {
    tree: TreeReader<'a>,
    /// Node budget for a single tree operation.
    max_nodes: usize,
}

impl<'a> FsReader<'a> {
    pub fn new(
        dev: &'a mut ReadOnlyDevice,
        map: &'a ChunkMap,
        base: u64,
        nodesize: u32,
    ) -> Self {
        Self { tree: TreeReader::new(dev, map, base, nodesize), max_nodes: 200_000 }
    }

    pub fn with_node_budget(mut self, max_nodes: usize) -> Self {
        self.max_nodes = max_nodes;
        self
    }

    /// List the subvolumes named in the root tree.
    ///
    /// Pairs each `ROOT_REF` (which carries the name) with its `ROOT_ITEM`
    /// (which carries the tree address and flags).
    pub fn subvolumes(&mut self, root_tree: u64) -> Result<Vec<Subvolume>, BtrfsError> {
        let items = self.tree.walk_leaves(root_tree, self.max_nodes)?;

        let mut names: Vec<(u64, String)> = Vec::new();
        for it in &items {
            if it.key.key_type == ROOT_REF_KEY {
                if let Some(n) = parse_root_ref_name(&it.data) {
                    names.push((it.key.offset, n));
                }
            }
        }

        let mut out = Vec::new();
        for it in &items {
            if it.key.key_type != ROOT_ITEM_KEY {
                continue;
            }
            let id = it.key.objectid;
            // Skip the internal trees; only user subvolumes are interesting.
            if id != FS_TREE_OBJECTID && id < FIRST_FREE_OBJECTID {
                continue;
            }
            let Some((root_bytenr, root_dirid, flags)) = parse_root_item(&it.data) else {
                continue;
            };
            let name = names
                .iter()
                .find(|(rid, _)| *rid == id)
                .map(|(_, n)| n.clone())
                .unwrap_or_else(|| {
                    if id == FS_TREE_OBJECTID { "<fs_tree>".into() } else { format!("<{id}>") }
                });
            out.push(Subvolume { id, name, root_bytenr, root_dirid, flags });
        }
        out.sort_by_key(|s| s.id);
        out.dedup_by_key(|s| s.id);
        Ok(out)
    }

    /// Read one inode from a subvolume's tree.
    pub fn inode(&mut self, subvol_root: u64, inode: u64) -> Result<Option<Inode>, BtrfsError> {
        let items = self.tree.find_items(subvol_root, inode, self.max_nodes)?;
        Ok(items
            .iter()
            .find(|i| i.key.key_type == INODE_ITEM_KEY)
            .and_then(|i| Inode::parse(&i.data)))
    }

    /// List the entries of a directory.
    ///
    /// `DIR_INDEX` items are used rather than `DIR_ITEM`, because a single
    /// `DIR_ITEM` key can hold several names that hash the same, while
    /// `DIR_INDEX` holds exactly one entry each and preserves order.
    pub fn read_dir(
        &mut self,
        subvol_root: u64,
        dir_inode: u64,
    ) -> Result<Vec<DirEntry>, BtrfsError> {
        let items = self.tree.find_items(subvol_root, dir_inode, self.max_nodes)?;
        let mut out = Vec::new();
        for it in items {
            match it.key.key_type {
                DIR_INDEX_KEY => out.extend(parse_dir_items(&it.data)),
                // A DIR_ITEM can chain multiple entries in one payload.
                DIR_ITEM_KEY if out.is_empty() => out.extend(parse_dir_items(&it.data)),
                _ => {}
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out.dedup_by(|a, b| a.name == b.name);
        Ok(out)
    }

    /// Collect a file's extents, ordered by offset within the file.
    pub fn extents(
        &mut self,
        subvol_root: u64,
        inode: u64,
    ) -> Result<Vec<Extent>, BtrfsError> {
        let items = self.tree.find_items(subvol_root, inode, self.max_nodes)?;
        let mut out = Vec::new();
        for it in items {
            if it.key.key_type != EXTENT_DATA_KEY {
                continue;
            }
            if let Some(e) = parse_file_extent(&it.data, it.key.offset) {
                out.push(e);
            }
        }
        out.sort_by_key(|e| match e {
            Extent::Inline { .. } => 0,
            Extent::Regular { file_offset, .. } | Extent::Hole { file_offset, .. } => *file_offset,
        });
        Ok(out)
    }

    /// Write a file's bytes to `out`, one extent at a time.
    ///
    /// Streaming matters here: a recovered video is routinely several
    /// gigabytes, and `read_file` would hold all of it in memory at once.
    /// `progress` is called with the running byte count so a UI can show it.
    pub fn extract_file(
        &mut self,
        subvol_root: u64,
        inode: u64,
        size: u64,
        out: &mut impl std::io::Write,
        mut progress: impl FnMut(u64),
    ) -> Result<u64, BtrfsError> {
        let extents = self.extents(subvol_root, inode)?;
        // Extents may leave gaps (holes); track where we are so they can be
        // written as zeroes rather than silently shortening the file.
        let mut written: u64 = 0;

        let write_zeroes = |out: &mut dyn std::io::Write, mut n: u64| -> std::io::Result<()> {
            const CHUNK: usize = 1 << 20;
            let zeros = vec![0u8; CHUNK.min(n.max(1) as usize)];
            while n > 0 {
                let take = (n as usize).min(zeros.len());
                out.write_all(&zeros[..take])?;
                n -= take as u64;
            }
            Ok(())
        };

        for e in extents {
            match e {
                Extent::Inline { data } => {
                    let take = (data.len() as u64).min(size.saturating_sub(written)) as usize;
                    out.write_all(&data[..take]).map_err(io_err)?;
                    written += take as u64;
                    progress(written);
                }
                Extent::Hole { file_offset, num_bytes } => {
                    if file_offset > written {
                        write_zeroes(out, file_offset - written).map_err(io_err)?;
                        written = file_offset;
                    }
                    let take = num_bytes.min(size.saturating_sub(written));
                    write_zeroes(out, take).map_err(io_err)?;
                    written += take;
                    progress(written);
                }
                Extent::Regular { file_offset, disk_bytenr, data_offset, num_bytes, compression } => {
                    if compression != 0 {
                        return Err(BtrfsError::Unsupported(format!(
                            "this file uses compression type {compression}, which is not decoded yet"
                        )));
                    }
                    // A gap before this extent is a hole.
                    if file_offset > written {
                        write_zeroes(out, file_offset - written).map_err(io_err)?;
                        written = file_offset;
                    }
                    if disk_bytenr == 0 {
                        let take = num_bytes.min(size.saturating_sub(written));
                        write_zeroes(out, take).map_err(io_err)?;
                        written += take;
                        progress(written);
                        continue;
                    }

                    // Copy in bounded pieces so memory stays flat.
                    const PIECE: u64 = 8 << 20;
                    let mut left = num_bytes.min(size.saturating_sub(written));
                    let mut at = disk_bytenr + data_offset;
                    while left > 0 {
                        let take = left.min(PIECE);
                        let bytes = self.tree.read_logical(at, take as usize)?;
                        out.write_all(&bytes).map_err(io_err)?;
                        at += take;
                        left -= take;
                        written += take;
                        progress(written);
                    }
                }
            }
        }

        // A file can end in a hole that has no extent item at all.
        if written < size {
            write_zeroes(out, size - written).map_err(io_err)?;
            written = size;
            progress(written);
        }
        Ok(written)
    }

    /// Read a whole file's bytes.
    ///
    /// Compressed extents are reported as unsupported rather than returned as
    /// raw compressed bytes, so a caller can never mistake one for file data.
    pub fn read_file(
        &mut self,
        subvol_root: u64,
        inode: u64,
        size: u64,
    ) -> Result<Vec<u8>, BtrfsError> {
        let extents = self.extents(subvol_root, inode)?;
        let mut out = vec![0u8; size as usize];

        for e in extents {
            match e {
                Extent::Inline { data } => {
                    let n = data.len().min(out.len());
                    out[..n].copy_from_slice(&data[..n]);
                }
                Extent::Hole { .. } => {} // already zero
                Extent::Regular { file_offset, disk_bytenr, data_offset, num_bytes, compression } => {
                    if disk_bytenr == 0 {
                        continue; // a hole expressed as a regular extent
                    }
                    if compression != 0 {
                        return Err(BtrfsError::Unsupported(format!(
                            "inode {inode} uses compression type {compression}, which this reader cannot decode yet"
                        )));
                    }
                    let start = file_offset as usize;
                    if start >= out.len() {
                        continue;
                    }
                    let want = (num_bytes as usize).min(out.len() - start);
                    let bytes = self.tree.read_logical(disk_bytenr + data_offset, want)?;
                    out[start..start + bytes.len()].copy_from_slice(&bytes);
                }
            }
        }
        Ok(out)
    }
}

fn parse_root_ref_name(d: &[u8]) -> Option<String> {
    // dirid(8) sequence(8) name_len(2) then the name.
    if d.len() < 18 {
        return None;
    }
    let n = le_u16(d, 16) as usize;
    let end = 18 + n;
    if end > d.len() {
        return None;
    }
    Some(String::from_utf8_lossy(&d[18..end]).into_owned())
}

/// Returns (bytenr, root_dirid, flags) from a root item.
fn parse_root_item(d: &[u8]) -> Option<(u64, u64, u64)> {
    // struct btrfs_inode_item is 160 bytes, then generation(8) root_dirid(8)
    // bytenr(8) byte_limit(8) bytes_used(8) last_snapshot(8) flags(8).
    const INODE_LEN: usize = 160;
    if d.len() < INODE_LEN + 56 {
        return None;
    }
    let root_dirid = le_u64(d, INODE_LEN + 8);
    let bytenr = le_u64(d, INODE_LEN + 16);
    let flags = le_u64(d, INODE_LEN + 48);
    Some((bytenr, root_dirid, flags))
}

/// A dir item payload may chain several entries back to back.
fn parse_dir_items(d: &[u8]) -> Vec<DirEntry> {
    const HEAD: usize = 30; // location(17) transid(8) data_len(2) name_len(2) type(1)
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + HEAD <= d.len() {
        let objectid = le_u64(d, at);
        let key_type = d[at + 8];
        let data_len = le_u16(d, at + 25) as usize;
        let name_len = le_u16(d, at + 27) as usize;
        let ftype = d[at + 29];
        let name_at = at + HEAD;
        if name_at + name_len > d.len() {
            break;
        }
        let name = String::from_utf8_lossy(&d[name_at..name_at + name_len]).into_owned();
        // A location key of type ROOT_ITEM points at a subvolume, not an inode.
        let is_subvol = key_type == ROOT_ITEM_KEY;
        out.push(DirEntry {
            name,
            inode: if is_subvol { 0 } else { objectid },
            entry_type: EntryType::from_raw(ftype),
            subvolume: is_subvol.then_some(objectid),
        });
        at = name_at + name_len + data_len;
    }
    out
}

fn parse_file_extent(d: &[u8], file_offset: u64) -> Option<Extent> {
    // generation(8) ram_bytes(8) compression(1) encryption(1)
    // other_encoding(2) type(1) = 21 bytes of header.
    const HEAD: usize = 21;
    if d.len() < HEAD {
        return None;
    }
    let ram_bytes = le_u64(d, 8);
    let compression = d[16];
    let extent_type = d[20];

    if extent_type == 0 {
        // Inline: the data follows the header directly.
        if compression != 0 {
            return None; // reported as unsupported by the caller
        }
        let end = d.len().min(HEAD + ram_bytes as usize);
        return Some(Extent::Inline { data: d[HEAD..end].to_vec() });
    }

    if d.len() < HEAD + 32 {
        return None;
    }
    let disk_bytenr = le_u64(d, HEAD);
    let data_offset = le_u64(d, HEAD + 16);
    let num_bytes = le_u64(d, HEAD + 24);

    if disk_bytenr == 0 {
        return Some(Extent::Hole { file_offset, num_bytes });
    }
    Some(Extent::Regular { file_offset, disk_bytenr, data_offset, num_bytes, compression })
}

fn io_err(e: std::io::Error) -> BtrfsError {
    BtrfsError::Unsupported(format!("write failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inode_bytes(size: u64, mode: u32) -> Vec<u8> {
        let mut d = vec![0u8; 160];
        d[16..24].copy_from_slice(&size.to_le_bytes());
        d[40..44].copy_from_slice(&1u32.to_le_bytes()); // nlink
        d[52..56].copy_from_slice(&mode.to_le_bytes());
        d
    }

    #[test]
    fn reads_size_and_mode_from_an_inode() {
        let i = Inode::parse(&inode_bytes(4096, 0o100644)).unwrap();
        assert_eq!(i.size, 4096);
        assert!(i.is_file());
        assert!(!i.is_dir());

        let d = Inode::parse(&inode_bytes(0, 0o040755)).unwrap();
        assert!(d.is_dir());

        let l = Inode::parse(&inode_bytes(12, 0o120777)).unwrap();
        assert!(l.is_symlink());
    }

    #[test]
    fn inode_field_offsets_stay_inside_the_struct() {
        // A wrong mtime offset previously read past the end of the item.
        let mut d = inode_bytes(0, 0o100644);
        d[136..144].copy_from_slice(&1_700_000_000u64.to_le_bytes());
        let i = Inode::parse(&d).unwrap();
        assert_eq!(i.mtime_secs, 1_700_000_000);
    }

    #[test]
    fn a_truncated_inode_is_rejected_rather_than_read_as_zeroes() {
        assert!(Inode::parse(&vec![0u8; 40]).is_none());
    }

    fn dir_item(objectid: u64, key_type: u8, ftype: u8, name: &str) -> Vec<u8> {
        let mut d = vec![0u8; 30];
        d[0..8].copy_from_slice(&objectid.to_le_bytes());
        d[8] = key_type;
        d[27..29].copy_from_slice(&(name.len() as u16).to_le_bytes());
        d[29] = ftype;
        d.extend_from_slice(name.as_bytes());
        d
    }

    #[test]
    fn parses_a_directory_entry() {
        let e = parse_dir_items(&dir_item(257, INODE_ITEM_KEY, 2, "Medias"));
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].name, "Medias");
        assert_eq!(e[0].inode, 257);
        assert_eq!(e[0].entry_type, EntryType::Dir);
        assert_eq!(e[0].subvolume, None);
    }

    #[test]
    fn an_entry_pointing_at_a_subvolume_is_marked_as_one() {
        // Synology's shared folders are subvolumes, so this distinction
        // decides whether the UI descends into an inode or another tree.
        let e = parse_dir_items(&dir_item(267, ROOT_ITEM_KEY, 2, "volume1"));
        assert_eq!(e[0].subvolume, Some(267));
        assert_eq!(e[0].inode, 0);
    }

    #[test]
    fn several_entries_in_one_payload_are_all_returned() {
        let mut d = dir_item(300, INODE_ITEM_KEY, 1, "a.txt");
        d.extend(dir_item(301, INODE_ITEM_KEY, 1, "b.txt"));
        let e = parse_dir_items(&d);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].name, "a.txt");
        assert_eq!(e[1].name, "b.txt");
    }

    #[test]
    fn a_name_running_past_the_payload_stops_the_walk() {
        let mut d = dir_item(1, INODE_ITEM_KEY, 1, "x");
        // Claim a name far longer than the bytes present.
        d[27..29].copy_from_slice(&9999u16.to_le_bytes());
        assert!(parse_dir_items(&d).is_empty());
    }

    fn extent_bytes(ty: u8, compression: u8, disk_bytenr: u64, num_bytes: u64) -> Vec<u8> {
        let mut d = vec![0u8; 53];
        d[8..16].copy_from_slice(&num_bytes.to_le_bytes()); // ram_bytes
        d[16] = compression;
        d[20] = ty;
        d[21..29].copy_from_slice(&disk_bytenr.to_le_bytes());
        d[37..45].copy_from_slice(&0u64.to_le_bytes()); // data offset
        d[45..53].copy_from_slice(&num_bytes.to_le_bytes());
        d
    }

    #[test]
    fn a_regular_extent_carries_its_disk_location() {
        match parse_file_extent(&extent_bytes(1, 0, 1 << 30, 4096), 0).unwrap() {
            Extent::Regular { disk_bytenr, num_bytes, .. } => {
                assert_eq!(disk_bytenr, 1 << 30);
                assert_eq!(num_bytes, 4096);
            }
            other => panic!("expected a regular extent, got {other:?}"),
        }
    }

    #[test]
    fn a_zero_disk_address_is_a_hole_not_a_read_at_offset_zero() {
        match parse_file_extent(&extent_bytes(1, 0, 0, 8192), 4096).unwrap() {
            Extent::Hole { file_offset, num_bytes } => {
                assert_eq!(file_offset, 4096);
                assert_eq!(num_bytes, 8192);
            }
            other => panic!("expected a hole, got {other:?}"),
        }
    }

    #[test]
    fn an_inline_extent_returns_its_bytes() {
        let mut d = vec![0u8; 21];
        d[8..16].copy_from_slice(&5u64.to_le_bytes()); // ram_bytes
        d[20] = 0; // inline
        d.extend_from_slice(b"hello");
        match parse_file_extent(&d, 0).unwrap() {
            Extent::Inline { data } => assert_eq!(data, b"hello"),
            other => panic!("expected inline, got {other:?}"),
        }
    }

    #[test]
    fn root_item_fields_are_read_from_the_right_offsets() {
        let mut d = vec![0u8; 400];
        d[160 + 8..160 + 16].copy_from_slice(&256u64.to_le_bytes()); // root_dirid
        d[160 + 16..160 + 24].copy_from_slice(&(1u64 << 32).to_le_bytes()); // bytenr
        d[160 + 48..160 + 56].copy_from_slice(&0x4_0000_0000u64.to_le_bytes()); // flags
        let (bytenr, dirid, flags) = parse_root_item(&d).unwrap();
        assert_eq!(dirid, 256);
        assert_eq!(bytenr, 1 << 32);
        // The Synology bit 34 lives in this field.
        assert_eq!(flags, 0x400000000);
    }
}
