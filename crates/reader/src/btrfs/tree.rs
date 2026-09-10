//! Btrfs B-tree traversal.
//!
//! A node is `nodesize` bytes: a header, then either key/blockptr pairs
//! (internal, `level > 0`) or items with data packed from the end of the node
//! (leaf, `level == 0`).

use super::*;
use crate::device::{le_u32, le_u64, ReadOnlyDevice};

/// Header offsets, shared with the superblock's first four fields.
mod off {
    use super::{CSUM_SIZE, FSID_SIZE, UUID_SIZE};
    pub const FSID: usize = CSUM_SIZE; // 32
    pub const BYTENR: usize = FSID + FSID_SIZE; // 48
    pub const FLAGS: usize = BYTENR + 8; // 56
    pub const CHUNK_TREE_UUID: usize = FLAGS + 8; // 64
    pub const GENERATION: usize = CHUNK_TREE_UUID + UUID_SIZE; // 80
    pub const OWNER: usize = GENERATION + 8; // 88
    pub const NRITEMS: usize = OWNER + 8; // 96
    pub const LEVEL: usize = NRITEMS + 4; // 100
    pub const END: usize = LEVEL + 1; // 101
}

/// A key identifies an item and orders the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    pub objectid: u64,
    pub key_type: u8,
    pub offset: u64,
}

impl Key {
    pub const fn new(objectid: u64, key_type: u8, offset: u64) -> Self {
        Self { objectid, key_type, offset }
    }

    fn parse(b: &[u8], at: usize) -> Self {
        Self {
            objectid: le_u64(b, at),
            key_type: b[at + 8],
            offset: le_u64(b, at + 9),
        }
    }
}

/// Packed size of `struct btrfs_disk_key`.
pub const KEY_LEN: usize = 17;
/// `struct btrfs_item` = key + offset(4) + size(4).
pub const ITEM_LEN: usize = KEY_LEN + 8;
/// `struct btrfs_key_ptr` = key + blockptr(8) + generation(8).
pub const KEY_PTR_LEN: usize = KEY_LEN + 16;

/// One item in a leaf, with its data.
#[derive(Debug, Clone)]
pub struct Item {
    pub key: Key,
    pub data: Vec<u8>,
}

/// A parsed node.
#[derive(Debug, Clone)]
pub enum Node {
    Internal { level: u8, children: Vec<(Key, u64)> },
    Leaf { items: Vec<Item> },
}

impl Node {
    pub fn parse(b: &[u8], logical: u64) -> Result<Self, BtrfsError> {
        if b.len() < off::END {
            return Err(BtrfsError::MalformedNode(logical, "node smaller than header".into()));
        }
        let nritems = le_u32(b, off::NRITEMS) as usize;
        let level = b[off::LEVEL];

        if level > 0 {
            let need = off::END + nritems * KEY_PTR_LEN;
            if need > b.len() {
                return Err(BtrfsError::MalformedNode(
                    logical,
                    format!("{nritems} key pointers do not fit in {} bytes", b.len()),
                ));
            }
            let mut children = Vec::with_capacity(nritems);
            for i in 0..nritems {
                let at = off::END + i * KEY_PTR_LEN;
                children.push((Key::parse(b, at), le_u64(b, at + KEY_LEN)));
            }
            return Ok(Node::Internal { level, children });
        }

        let need = off::END + nritems * ITEM_LEN;
        if need > b.len() {
            return Err(BtrfsError::MalformedNode(
                logical,
                format!("{nritems} items do not fit in {} bytes", b.len()),
            ));
        }
        let mut items = Vec::with_capacity(nritems);
        for i in 0..nritems {
            let at = off::END + i * ITEM_LEN;
            let key = Key::parse(b, at);
            // Item data offsets are relative to the end of the header.
            let data_off = off::END + le_u32(b, at + KEY_LEN) as usize;
            let data_len = le_u32(b, at + KEY_LEN + 4) as usize;
            if data_off + data_len > b.len() {
                return Err(BtrfsError::MalformedNode(
                    logical,
                    format!("item {i} data [{data_off}..+{data_len}] out of bounds"),
                ));
            }
            items.push(Item { key, data: b[data_off..data_off + data_len].to_vec() });
        }
        Ok(Node::Leaf { items })
    }
}

/// Reads tree nodes, translating logical addresses through a [`ChunkMap`].
pub struct TreeReader<'a> {
    dev: &'a mut ReadOnlyDevice,
    map: &'a ChunkMap,
    /// Byte offset of the filesystem within the device.
    base: u64,
    nodesize: u32,
}

impl<'a> TreeReader<'a> {
    pub fn new(
        dev: &'a mut ReadOnlyDevice,
        map: &'a ChunkMap,
        base: u64,
        nodesize: u32,
    ) -> Self {
        Self { dev, map, base, nodesize }
    }

    /// Read and parse the node at a logical address.
    pub fn read_node(&mut self, logical: u64) -> Result<Node, BtrfsError> {
        let (physical, run) = self.map.resolve(logical)?;
        if run < self.nodesize as u64 {
            return Err(BtrfsError::MalformedNode(
                logical,
                "node straddles the end of its chunk".into(),
            ));
        }
        let bytes = self.dev.read_at(self.base + physical, self.nodesize as usize)?;
        Node::parse(&bytes, logical)
    }

    /// Read raw bytes at a logical address, following chunk boundaries.
    ///
    /// A read may span several chunks, so it is issued piecewise rather than
    /// assuming the whole range is contiguous on the device.
    pub fn read_logical(&mut self, logical: u64, len: usize) -> Result<Vec<u8>, BtrfsError> {
        let mut out = Vec::with_capacity(len);
        let mut at = logical;
        let mut left = len;
        while left > 0 {
            let (physical, run) = self.map.resolve(at)?;
            let take = (run as usize).min(left);
            out.extend(self.dev.read_at(self.base + physical, take)?);
            at += take as u64;
            left -= take;
        }
        Ok(out)
    }

    /// Collect every leaf item in the tree rooted at `root`.
    ///
    /// `max_nodes` bounds the walk so a corrupt tree cannot loop forever.
    pub fn walk_leaves(
        &mut self,
        root: u64,
        max_nodes: usize,
    ) -> Result<Vec<Item>, BtrfsError> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        let mut seen = std::collections::HashSet::new();
        let mut visited = 0usize;

        while let Some(logical) = stack.pop() {
            if !seen.insert(logical) {
                continue; // a cycle, or a node reached twice
            }
            visited += 1;
            if visited > max_nodes {
                return Err(BtrfsError::Unsupported(format!(
                    "tree walk exceeded {max_nodes} nodes; refusing to continue"
                )));
            }
            match self.read_node(logical)? {
                Node::Leaf { items } => out.extend(items),
                Node::Internal { children, .. } => {
                    stack.extend(children.into_iter().map(|(_, ptr)| ptr));
                }
            }
        }
        Ok(out)
    }

    /// Walk only the parts of a tree that can contain `objectid`.
    ///
    /// Descends by key order rather than visiting every node, which matters on
    /// a multi-terabyte filesystem tree.
    pub fn find_items(
        &mut self,
        root: u64,
        objectid: u64,
        max_nodes: usize,
    ) -> Result<Vec<Item>, BtrfsError> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        let mut seen = std::collections::HashSet::new();
        let mut visited = 0usize;

        while let Some(logical) = stack.pop() {
            if !seen.insert(logical) {
                continue;
            }
            visited += 1;
            if visited > max_nodes {
                return Err(BtrfsError::Unsupported(format!(
                    "search exceeded {max_nodes} nodes; refusing to continue"
                )));
            }
            match self.read_node(logical)? {
                Node::Leaf { items } => {
                    out.extend(items.into_iter().filter(|i| i.key.objectid == objectid));
                }
                Node::Internal { children, .. } => {
                    // A child covers keys from its own key up to the next
                    // child's key, so descend where objectid could live.
                    for i in 0..children.len() {
                        let lo = children[i].0.objectid;
                        let hi = children.get(i + 1).map(|c| c.0.objectid);
                        let could_contain =
                            lo <= objectid && hi.map_or(true, |h| objectid <= h);
                        if could_contain {
                            stack.push(children[i].1);
                        }
                    }
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(items: &[(Key, &[u8])], nodesize: usize) -> Vec<u8> {
        let mut b = vec![0u8; nodesize];
        b[off::NRITEMS..off::NRITEMS + 4].copy_from_slice(&(items.len() as u32).to_le_bytes());
        b[off::LEVEL] = 0;
        // Data is packed from the end of the node backwards.
        let mut data_end = nodesize - off::END;
        for (i, (key, data)) in items.iter().enumerate() {
            let at = off::END + i * ITEM_LEN;
            b[at..at + 8].copy_from_slice(&key.objectid.to_le_bytes());
            b[at + 8] = key.key_type;
            b[at + 9..at + 17].copy_from_slice(&key.offset.to_le_bytes());
            data_end -= data.len();
            b[at + KEY_LEN..at + KEY_LEN + 4].copy_from_slice(&(data_end as u32).to_le_bytes());
            b[at + KEY_LEN + 4..at + KEY_LEN + 8]
                .copy_from_slice(&(data.len() as u32).to_le_bytes());
            let abs = off::END + data_end;
            b[abs..abs + data.len()].copy_from_slice(data);
        }
        b
    }

    fn internal(children: &[(Key, u64)], nodesize: usize) -> Vec<u8> {
        let mut b = vec![0u8; nodesize];
        b[off::NRITEMS..off::NRITEMS + 4]
            .copy_from_slice(&(children.len() as u32).to_le_bytes());
        b[off::LEVEL] = 1;
        for (i, (key, ptr)) in children.iter().enumerate() {
            let at = off::END + i * KEY_PTR_LEN;
            b[at..at + 8].copy_from_slice(&key.objectid.to_le_bytes());
            b[at + 8] = key.key_type;
            b[at + 9..at + 17].copy_from_slice(&key.offset.to_le_bytes());
            b[at + KEY_LEN..at + KEY_LEN + 8].copy_from_slice(&ptr.to_le_bytes());
        }
        b
    }

    #[test]
    fn parses_a_leaf_and_recovers_item_data() {
        let items = [
            (Key::new(256, INODE_ITEM_KEY, 0), b"first".as_slice()),
            (Key::new(257, DIR_ITEM_KEY, 42), b"second".as_slice()),
        ];
        match Node::parse(&leaf(&items, 4096), 0).unwrap() {
            Node::Leaf { items } => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].key, Key::new(256, INODE_ITEM_KEY, 0));
                assert_eq!(items[0].data, b"first");
                assert_eq!(items[1].data, b"second");
            }
            other => panic!("expected a leaf, got {other:?}"),
        }
    }

    #[test]
    fn parses_an_internal_node_into_child_pointers() {
        let kids = [(Key::new(1, 0, 0), 8192u64), (Key::new(500, 0, 0), 16384u64)];
        match Node::parse(&internal(&kids, 4096), 0).unwrap() {
            Node::Internal { level, children } => {
                assert_eq!(level, 1);
                assert_eq!(children, vec![(Key::new(1, 0, 0), 8192), (Key::new(500, 0, 0), 16384)]);
            }
            other => panic!("expected an internal node, got {other:?}"),
        }
    }

    #[test]
    fn an_item_pointing_outside_the_node_is_rejected() {
        let mut b = leaf(&[(Key::new(1, 1, 0), b"x")], 4096);
        // Point the item's data past the end of the node.
        let at = off::END;
        b[at + KEY_LEN..at + KEY_LEN + 4].copy_from_slice(&(u32::MAX - 10).to_le_bytes());
        assert!(matches!(Node::parse(&b, 99), Err(BtrfsError::MalformedNode(99, _))));
    }

    #[test]
    fn an_impossible_item_count_is_rejected() {
        let mut b = vec![0u8; 4096];
        b[off::NRITEMS..off::NRITEMS + 4].copy_from_slice(&10_000u32.to_le_bytes());
        b[off::LEVEL] = 0;
        assert!(Node::parse(&b, 7).is_err());
    }

    #[test]
    fn keys_order_by_objectid_then_type_then_offset() {
        let mut keys = vec![
            Key::new(2, 1, 0),
            Key::new(1, 200, 5),
            Key::new(1, 1, 9),
            Key::new(1, 1, 0),
        ];
        keys.sort();
        assert_eq!(
            keys,
            vec![Key::new(1, 1, 0), Key::new(1, 1, 9), Key::new(1, 200, 5), Key::new(2, 1, 0)]
        );
    }
}
