//! Opening a Synology disk and browsing it, for the UI.
//!
//! Wraps `mbs_reader` so the frontend deals in paths and entries rather than
//! logical addresses. Everything here is read-only by construction: the reader
//! crate has no write path at all.

use mbs_reader::btrfs::{chunk, fs::FsReader, tree, ChunkMap, Superblock};
use mbs_reader::{lvm, mdadm, ReadOnlyDevice};
use serde::{Deserialize, Serialize};

/// Everything needed to answer later browse calls without re-scanning.
pub struct OpenDisk {
    pub device: String,
    /// The authorized handle, kept open so the user is prompted once per disk
    /// rather than once per directory listing.
    dev: ReadOnlyDevice,
    /// Byte offset of the Btrfs filesystem within the device.
    pub fs_base: u64,
    pub nodesize: u32,
    pub chunks: ChunkMap,
    pub root_tree: u64,
    pub label: String,
    pub fsid: String,
    pub bytes_used: u64,
    pub subvolumes: Vec<SubvolInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubvolInfo {
    pub id: u64,
    pub name: String,
    pub root_bytenr: u64,
    pub root_dirid: u64,
    /// Raw Synology flags, rendered as hex for display.
    pub flags_hex: String,
    /// True when this looks like DSM's own bookkeeping rather than user data.
    pub system: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub name: String,
    pub inode: u64,
    /// "dir", "file", "symlink" or "other".
    pub kind: String,
    pub size: u64,
    /// Set when the entry crosses into another subvolume.
    pub subvolume: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum BrowseError {
    #[error("cannot open {0}: {1}")]
    Open(String, String),
    #[error("no Synology RAID metadata on this partition: {0}")]
    NoRaid(String),
    #[error("this disk is part of a {0:?} array and cannot be read from one member")]
    NeedsAllMembers(String),
    #[error("no LVM volume group: {0}")]
    NoLvm(String),
    #[error("no Btrfs filesystem: {0}")]
    NoBtrfs(String),
    #[error("{0}")]
    Read(String),
}

/// Names DSM uses for its own subvolumes; these are hidden by default so the
/// user sees their shared folders rather than 20 lines of `@`-prefixed noise.
fn looks_like_system(name: &str, flags: u64) -> bool {
    name.starts_with('@')
        || name == "Snapshot"
        || name.starts_with('<')
        // UUID-named and Docker-layer subvolumes carry the private bits.
        || (flags != 0 && (name.len() == 36 || name.len() == 64))
}

/// Open a Synology disk: walk RAID → LVM → Btrfs and list its subvolumes.
pub fn open(device: &str) -> Result<OpenDisk, BrowseError> {
    // Shows the system password dialog when the raw node is not readable.
    let mut dev = ReadOnlyDevice::open_with_authorization(device)
        .map_err(|e| BrowseError::Open(device.into(), e.to_string()))?;

    // Layer 1: RAID. A mirror member carries the whole array.
    let md = mdadm::read_superblock(&mut dev).map_err(|e| BrowseError::NoRaid(e.to_string()))?;
    if !md.is_single_member_readable() {
        return Err(BrowseError::NeedsAllMembers(format!("{:?}", md.level)));
    }
    let base = md.data_offset_bytes;

    // Layer 2: LVM.
    let label = lvm::read_label(&mut dev, base).map_err(|e| BrowseError::NoLvm(e.to_string()))?;
    let mda = *label
        .metadata_areas
        .first()
        .ok_or_else(|| BrowseError::NoLvm("no metadata area".into()))?;
    let text = lvm::read_vg_metadata(&mut dev, base, mda)
        .map_err(|e| BrowseError::NoLvm(e.to_string()))?;
    let vg = lvm::parse_vg_metadata(&text).map_err(|e| BrowseError::NoLvm(e.to_string()))?;
    if vg.logical_volumes.is_empty() {
        return Err(BrowseError::NoLvm("volume group has no logical volume".into()));
    }
    // Prefer pe_start from the VG text; fall back to the PV header's data
    // area when the metadata does not record one.
    let data_area = if vg.pe_start_bytes != 0 {
        vg.pe_start_bytes
    } else {
        label.data_areas.first().map(|d| d.offset).unwrap_or(0)
    };

    // A volume group can hold several logical volumes, and only one of them
    // carries the shared folders. Rather than assuming the first is the right
    // one, look for the LV whose start actually holds a Btrfs superblock.
    let mut fs_base = 0u64;
    let mut sb = None;
    let mut tried = Vec::new();
    for lv in &vg.logical_volumes {
        let Some((phys, _)) = lv.map_offset(0, vg.extent_size_bytes) else {
            continue;
        };
        let candidate = base + data_area + phys;
        match Superblock::read(&mut dev, candidate) {
            Ok(found) => {
                fs_base = candidate;
                sb = Some(found);
                break;
            }
            Err(e) => tried.push(format!(
                "{} @ {} ({} extents): {e}",
                lv.name,
                candidate,
                lv.segments.iter().map(|s| s.extent_count).sum::<u64>()
            )),
        }
    }
    let sb = sb.ok_or_else(|| {
        BrowseError::NoBtrfs(format!(
            "ไม่พบ Btrfs ใน logical volume ทั้ง {} ตัวของ {}\n\
             RAID data_offset {} · pe_start {} · extent {} bytes\n{}",
            vg.logical_volumes.len(),
            vg.name,
            base,
            data_area,
            vg.extent_size_bytes,
            tried.join("\n")
        ))
    })?;
    let mut chunks =
        ChunkMap::from_sys_array(&sb.sys_chunk_array).map_err(|e| BrowseError::Read(e.to_string()))?;
    {
        // The bootstrap array only covers the chunk tree; read the rest.
        let mut tr = tree::TreeReader::new(&mut dev, &chunks, fs_base, sb.nodesize);
        let items = tr
            .walk_leaves(sb.chunk_root, 100_000)
            .map_err(|e| BrowseError::Read(e.to_string()))?;
        let mut more = ChunkMap::new();
        for it in &items {
            if it.key.key_type == mbs_reader::btrfs::CHUNK_ITEM_KEY {
                if let Ok((c, _)) = chunk::parse_chunk(&it.data, 0, it.key.offset) {
                    more.insert(c);
                }
            }
        }
        for c in more.chunks() {
            chunks.insert(c.clone());
        }
    }

    let subvolumes = {
        let mut fs = FsReader::new(&mut dev, &chunks, fs_base, sb.nodesize);
        let subs = fs.subvolumes(sb.root).map_err(|e| BrowseError::Read(e.to_string()))?;
        subs.into_iter()
            .map(|s| SubvolInfo {
                system: looks_like_system(&s.name, s.flags),
                flags_hex: format!("0x{:x}", s.flags),
                id: s.id,
                name: s.name,
                root_bytenr: s.root_bytenr,
                root_dirid: s.root_dirid,
            })
            .collect()
    };

    Ok(OpenDisk {
        device: device.to_string(),
        dev,
        fs_base,
        nodesize: sb.nodesize,
        chunks,
        root_tree: sb.root,
        label: sb.label.clone(),
        fsid: sb.fsid_string(),
        bytes_used: sb.bytes_used,
        subvolumes,
    })
}

impl OpenDisk {
    pub fn subvolume(&self, id: u64) -> Option<&SubvolInfo> {
        self.subvolumes.iter().find(|s| s.id == id)
    }

    /// List a directory inside a subvolume.
    ///
    /// `path` is `/`-separated and relative to the subvolume root; an empty
    /// path lists the root itself.
    pub fn list(&mut self, subvol_id: u64, path: &str) -> Result<Vec<Entry>, BrowseError> {
        let sub = self
            .subvolume(subvol_id)
            .ok_or_else(|| BrowseError::Read(format!("no subvolume {subvol_id}")))?
            .clone();
        let mut fs = FsReader::new(&mut self.dev, &self.chunks, self.fs_base, self.nodesize);

        let mut inode = sub.root_dirid;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            let here = fs
                .read_dir(sub.root_bytenr, inode)
                .map_err(|e| BrowseError::Read(e.to_string()))?;
            let next = here
                .iter()
                .find(|e| e.name == part)
                .ok_or_else(|| BrowseError::Read(format!("{part:?} not found in {path:?}")))?;
            inode = next.inode;
        }

        let entries = fs
            .read_dir(sub.root_bytenr, inode)
            .map_err(|e| BrowseError::Read(e.to_string()))?;

        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            // A size lookup per entry is one tree search; acceptable for a
            // directory listing and it is what makes the UI useful.
            let size = if e.inode == 0 {
                0
            } else {
                fs.inode(sub.root_bytenr, e.inode)
                    .ok()
                    .flatten()
                    .map(|i| i.size)
                    .unwrap_or(0)
            };
            out.push(Entry {
                kind: match e.entry_type {
                    mbs_reader::btrfs::EntryType::Dir => "dir",
                    mbs_reader::btrfs::EntryType::File => "file",
                    mbs_reader::btrfs::EntryType::Symlink => "symlink",
                    mbs_reader::btrfs::EntryType::Other(_) => "other",
                }
                .to_string(),
                name: e.name,
                inode: e.inode,
                size,
                subvolume: e.subvolume,
            });
        }
        // Directories first, then by name, the way a file browser orders them.
        out.sort_by(|a, b| {
            (a.kind != "dir").cmp(&(b.kind != "dir")).then_with(|| a.name.cmp(&b.name))
        });
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsm_internal_subvolumes_are_marked_as_system() {
        // Names read from the real disk.
        assert!(looks_like_system("@syno", 0));
        assert!(looks_like_system("@iSCSI", 0));
        assert!(looks_like_system("Snapshot", 0x600000000));
        assert!(looks_like_system("<fs_tree>", 0));
        // A 36-char UUID subvolume with private flags.
        assert!(looks_like_system("9c1a3cd3-e896-434c-a1a7-19bb6c2b8bb3", 0xc00000000));
        // A 64-char Docker layer id.
        assert!(looks_like_system(
            "04d5db0d13f5fbe4695510c478940d0dc76f56064dad9ce9b91a333472d5d557",
            0x400000000
        ));
    }

    #[test]
    fn user_shared_folders_are_not_hidden() {
        // These are exactly what the user came to recover.
        for name in ["Picture", "photo", "video", "music", "Documents", "chat", "homes"] {
            assert!(!looks_like_system(name, 0), "{name} must stay visible");
        }
        // `web` carries a private flag but is still a user share.
        assert!(!looks_like_system("web", 0x400000000));
    }

    #[test]
    fn a_short_name_with_flags_is_not_mistaken_for_a_uuid() {
        // Length is only a UUID signal at exactly 36 or 64 characters.
        assert!(!looks_like_system("office", 0x400000000));
        assert!(!looks_like_system("homes", 0x400000000));
    }
}

/// Progress of one copy operation, polled by the UI.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CopyProgress {
    pub files_done: u64,
    pub files_total: u64,
    /// Files that already matched at the destination and were left alone.
    pub files_skipped: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub current: String,
    pub finished: bool,
    /// Non-fatal per-file failures, so one bad file does not abort the rest.
    pub errors: Vec<String>,
}

/// What to do when a planned file already exists at the destination.
///
/// The user asked for exactly these three choices, so a recovery that was
/// stopped partway through can be resumed without re-reading everything that
/// already landed intact, and without a silent guess either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    /// Always overwrite, even if the existing file looks identical.
    Overwrite,
    /// Never touch a path that already exists; leave it as is.
    Skip,
    /// Overwrite only when the existing file's size does not match the
    /// source. A same-size file is treated as already recovered — this is a
    /// resume, not a byte-for-byte verification.
    OverwriteIfDifferent,
}

/// Whether an existing destination file should be left as is.
///
/// Pulled out of `copy_out` so the three-way choice the user asked for —
/// always overwrite, never overwrite, or overwrite only when sizes differ —
/// is exercised directly by a test rather than only through a full disk copy.
fn keep_existing(policy: ConflictPolicy, source_size: u64, dest_size: u64) -> bool {
    match policy {
        ConflictPolicy::Overwrite => false,
        ConflictPolicy::Skip => true,
        ConflictPolicy::OverwriteIfDifferent => dest_size == source_size,
    }
}

/// One planned file: its subvolume-relative source path, inode, and size.
type PlannedFile = (u64, String, u64);

/// A conflict found while planning a copy, for the confirmation prompt.
#[derive(Debug, Clone, Serialize)]
pub struct Conflict {
    pub rel_path: String,
    pub source_size: u64,
    pub dest_size: u64,
}

/// What a copy would do, computed without touching the destination.
#[derive(Debug, Clone, Serialize)]
pub struct CopyPlan {
    pub files_total: u64,
    pub bytes_total: u64,
    /// Destination paths that already exist. Empty means nothing to decide.
    pub conflicts: Vec<Conflict>,
}

/// Names Synology stores for its own bookkeeping; skipped when copying a tree.
const SKIP_DIRS: &[&str] = &["@eaDir", "#recycle", "@tmp", "@sharesnap"];

impl OpenDisk {
    /// Resolve every requested path into a flat list of files to copy.
    ///
    /// Shared by `plan_copy` (read-only, for the confirmation dialog) and
    /// `copy_out` (the real thing), so the count the user confirms is exactly
    /// what gets written.
    fn build_plan(
        &mut self,
        subvol_id: u64,
        paths: &[String],
        dest_dir: &std::path::Path,
    ) -> Result<(Vec<(PlannedFile, std::path::PathBuf)>, SubvolInfo), BrowseError> {
        let sub = self
            .subvolume(subvol_id)
            .ok_or_else(|| BrowseError::Read(format!("no subvolume {subvol_id}")))?
            .clone();

        let mut plan = Vec::new();
        for path in paths {
            let (inode, is_dir, name) = self.resolve(&sub, path)?;
            let mut items = Vec::new();
            if is_dir {
                self.plan_tree(&sub, inode, &name, &mut items)?;
            } else {
                let size = self.size_of(&sub, inode)?;
                items.push((inode, name.clone(), size));
            }
            for item in items {
                let target = dest_dir.join(&item.1);
                plan.push((item, target));
            }
        }
        Ok((plan, sub))
    }

    /// Work out what a copy would do, without writing anything.
    ///
    /// The UI calls this first so it can ask "overwrite / skip / overwrite if
    /// different" only when there is something to decide, instead of always
    /// interrupting the user.
    pub fn plan_copy(
        &mut self,
        subvol_id: u64,
        paths: &[String],
        dest_dir: &std::path::Path,
    ) -> Result<CopyPlan, BrowseError> {
        let (plan, _) = self.build_plan(subvol_id, paths, dest_dir)?;
        let mut conflicts = Vec::new();
        for ((_, rel, size), target) in &plan {
            if let Ok(meta) = std::fs::metadata(target) {
                conflicts.push(Conflict {
                    rel_path: rel.clone(),
                    source_size: *size,
                    dest_size: meta.len(),
                });
            }
        }
        Ok(CopyPlan {
            files_total: plan.len() as u64,
            bytes_total: plan.iter().map(|((_, _, s), _)| *s).sum(),
            conflicts,
        })
    }

    /// Copy one or more files/folders to `dest_dir`, applying `policy` to any
    /// path that already exists there.
    ///
    /// Reads stream through a bounded buffer, so a multi-gigabyte video does
    /// not need to fit in memory. `report` is called as progress advances.
    pub fn copy_out(
        &mut self,
        subvol_id: u64,
        paths: &[String],
        dest_dir: &std::path::Path,
        policy: ConflictPolicy,
        report: &mut impl FnMut(CopyProgress),
    ) -> Result<CopyProgress, BrowseError> {
        let (plan, sub) = self.build_plan(subvol_id, paths, dest_dir)?;

        let mut progress = CopyProgress::default();
        progress.files_total = plan.len() as u64;
        progress.bytes_total = plan.iter().map(|((_, _, s), _)| *s).sum();
        report(progress.clone());

        for ((ino, rel, size), target) in plan {
            progress.current = rel.clone();
            report(progress.clone());

            // Decide once per file whether an existing copy should be kept.
            if let Ok(meta) = std::fs::metadata(&target) {
                if keep_existing(policy, size, meta.len()) {
                    progress.files_skipped += 1;
                    progress.files_done += 1;
                    progress.bytes_done += size;
                    report(progress.clone());
                    continue;
                }
            }

            if let Some(parent) = target.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    progress.errors.push(format!("{rel}: {e}"));
                    continue;
                }
            }

            let base_done = progress.bytes_done;
            match self.write_one(&sub, ino, size, &target, &mut |n| {
                let mut p = progress.clone();
                p.bytes_done = base_done + n;
                report(p);
            }) {
                Ok(n) => progress.bytes_done = base_done + n,
                Err(e) => {
                    // Keep going: a single unreadable file should not stop a
                    // recovery of thousands.
                    progress.errors.push(format!("{rel}: {e}"));
                    std::fs::remove_file(&target).ok();
                }
            }
            progress.files_done += 1;
            report(progress.clone());
        }

        progress.finished = true;
        progress.current.clear();
        report(progress.clone());
        Ok(progress)
    }

    fn resolve(&mut self, sub: &SubvolInfo, path: &str) -> Result<(u64, bool, String), BrowseError> {
        let mut fs = FsReader::new(&mut self.dev, &self.chunks, self.fs_base, self.nodesize);
        let mut inode = sub.root_dirid;
        let mut is_dir = true;
        let mut name = sub.name.clone();

        for part in path.split('/').filter(|p| !p.is_empty()) {
            let here = fs
                .read_dir(sub.root_bytenr, inode)
                .map_err(|e| BrowseError::Read(e.to_string()))?;
            let hit = here
                .iter()
                .find(|e| e.name == part)
                .ok_or_else(|| BrowseError::Read(format!("{part:?} not found")))?;
            inode = hit.inode;
            is_dir = matches!(hit.entry_type, mbs_reader::btrfs::EntryType::Dir);
            name = hit.name.clone();
        }
        Ok((inode, is_dir, name))
    }

    fn size_of(&mut self, sub: &SubvolInfo, inode: u64) -> Result<u64, BrowseError> {
        let mut fs = FsReader::new(&mut self.dev, &self.chunks, self.fs_base, self.nodesize);
        Ok(fs
            .inode(sub.root_bytenr, inode)
            .map_err(|e| BrowseError::Read(e.to_string()))?
            .map(|i| i.size)
            .unwrap_or(0))
    }

    /// Build the list of files under a directory, depth-first.
    fn plan_tree(
        &mut self,
        sub: &SubvolInfo,
        inode: u64,
        prefix: &str,
        out: &mut Vec<(u64, String, u64)>,
    ) -> Result<(), BrowseError> {
        let entries = {
            let mut fs = FsReader::new(&mut self.dev, &self.chunks, self.fs_base, self.nodesize);
            fs.read_dir(sub.root_bytenr, inode)
                .map_err(|e| BrowseError::Read(e.to_string()))?
        };
        for e in entries {
            if SKIP_DIRS.contains(&e.name.as_str()) || e.subvolume.is_some() {
                continue;
            }
            let rel = format!("{prefix}/{}", e.name);
            match e.entry_type {
                mbs_reader::btrfs::EntryType::Dir => {
                    self.plan_tree(sub, e.inode, &rel, out)?;
                }
                mbs_reader::btrfs::EntryType::File => {
                    let size = self.size_of(sub, e.inode)?;
                    out.push((e.inode, rel, size));
                }
                _ => {} // symlinks and specials are not reconstructed
            }
        }
        Ok(())
    }

    fn write_one(
        &mut self,
        sub: &SubvolInfo,
        inode: u64,
        size: u64,
        target: &std::path::Path,
        progress: &mut impl FnMut(u64),
    ) -> Result<u64, BrowseError> {
        use std::io::BufWriter;
        let file = std::fs::File::create(target)
            .map_err(|e| BrowseError::Read(format!("cannot create {}: {e}", target.display())))?;
        let mut out = BufWriter::with_capacity(1 << 20, file);
        let mut fs = FsReader::new(&mut self.dev, &self.chunks, self.fs_base, self.nodesize);
        let n = fs
            .extract_file(sub.root_bytenr, inode, size, &mut out, progress)
            .map_err(|e| BrowseError::Read(e.to_string()))?;
        use std::io::Write;
        out.flush().map_err(|e| BrowseError::Read(e.to_string()))?;
        Ok(n)
    }
}

/// One entry on the local (destination) side.
#[derive(Debug, Clone, Serialize)]
pub struct LocalEntry {
    pub name: String,
    pub kind: String,
    pub size: u64,
    pub path: String,
}

/// Where new recoveries land unless the user picks somewhere else.
///
/// `~/Downloads` is the folder people already know, so the app opens there
/// rather than making them choose a disk before anything can happen.
pub fn default_destination() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .map(|h| h.join("Downloads"))
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
}

/// List a local directory for the destination pane.
pub fn list_local(dir: &std::path::Path) -> Result<Vec<LocalEntry>, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Hidden files are noise in a destination picker.
        if name.starts_with('.') {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        out.push(LocalEntry {
            kind: if meta.is_dir() { "dir" } else { "file" }.to_string(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            path: entry.path().to_string_lossy().into_owned(),
            name,
        });
    }
    out.sort_by(|a, b| {
        (a.kind != "dir").cmp(&(b.kind != "dir")).then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}

/// Free space on the volume holding `dir`, for the "enough room?" check.
pub fn free_space(dir: &std::path::Path) -> u64 {
    use std::os::unix::ffi::OsStrExt;
    let mut path = dir.as_os_str().as_bytes().to_vec();
    path.push(0);
    // SAFETY: `path` is NUL-terminated and `stat` is written only by statfs.
    unsafe {
        let mut st: libc::statfs = std::mem::zeroed();
        if libc::statfs(path.as_ptr() as *const libc::c_char, &mut st) == 0 {
            st.f_bavail as u64 * st.f_bsize as u64
        } else {
            0
        }
    }
}

#[cfg(test)]
mod local_tests {
    use super::*;

    #[test]
    fn the_default_destination_is_the_users_downloads_folder() {
        let d = default_destination();
        assert!(d.ends_with("Downloads"), "got {}", d.display());
    }

    #[test]
    fn listing_skips_hidden_files() {
        let dir = std::env::temp_dir().join(format!("mbs-local-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("visible.txt"), b"hi").unwrap();
        std::fs::write(dir.join(".DS_Store"), b"x").unwrap();
        std::fs::create_dir_all(dir.join("folder")).unwrap();

        let out = list_local(&dir).unwrap();
        let names: Vec<_> = out.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["folder", "visible.txt"], "dirs sort first, dotfiles hidden");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn free_space_is_reported_for_a_real_directory() {
        assert!(free_space(&std::env::temp_dir()) > 0);
    }

    #[test]
    fn listing_a_missing_directory_is_an_error_not_an_empty_list() {
        assert!(list_local(std::path::Path::new("/no/such/dir")).is_err());
    }

    /// The overwrite policy is the part of a recovery a user relies on not to
    /// silently guess wrong, so each of the three choices gets its own case.
    #[test]
    fn overwrite_policy_always_replaces_the_existing_file() {
        assert!(!keep_existing(ConflictPolicy::Overwrite, 100, 100));
        assert!(!keep_existing(ConflictPolicy::Overwrite, 100, 50));
    }

    #[test]
    fn skip_policy_never_touches_the_existing_file() {
        assert!(keep_existing(ConflictPolicy::Skip, 100, 100));
        assert!(keep_existing(ConflictPolicy::Skip, 100, 50));
    }

    #[test]
    fn overwrite_if_different_keeps_a_matching_size_and_replaces_a_mismatch() {
        // Same size: treated as already recovered from an earlier run.
        assert!(keep_existing(ConflictPolicy::OverwriteIfDifferent, 100, 100));
        // Different size: the earlier copy was partial or wrong, replace it.
        assert!(!keep_existing(ConflictPolicy::OverwriteIfDifferent, 100, 50));
        assert!(!keep_existing(ConflictPolicy::OverwriteIfDifferent, 100, 150));
    }

}
