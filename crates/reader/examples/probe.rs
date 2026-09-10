use mbs_reader::btrfs::{chunk, fs::FsReader, tree, ChunkMap, Superblock};
use mbs_reader::{lvm, mdadm, ReadOnlyDevice};

fn main() {
    let path = std::env::args().nth(1).expect("usage: probe /dev/rdiskNsM [subvol_id] [path]");
    let want_subvol: Option<u64> = std::env::args().nth(2).and_then(|s| s.parse().ok());
    let want_path = std::env::args().nth(3);

    let mut dev = ReadOnlyDevice::open(&path).expect("open");

    let sb = mdadm::read_superblock(&mut dev).expect("mdadm");
    println!("[1] mdadm {:?} {:?}  data_offset {}", sb.level, sb.name, sb.data_offset_bytes);
    let base = sb.data_offset_bytes;

    let label = lvm::read_label(&mut dev, base).expect("lvm label");
    let mda = *label.metadata_areas.first().expect("mda");
    let text = lvm::read_vg_metadata(&mut dev, base, mda).expect("vg text");
    let vg = lvm::parse_vg_metadata(&text).expect("vg parse");
    let lv = vg.logical_volumes.first().expect("lv");
    let data_area = label.data_areas.first().map(|d| d.offset).unwrap_or(0);
    let (phys, _) = lv.map_offset(0, vg.extent_size_bytes).expect("lv map");
    let fs_base = base + data_area + phys;
    println!("[2] lvm {}/{}  btrfs at {}", vg.name, lv.name, fs_base);

    let bsb = Superblock::read(&mut dev, fs_base).expect("btrfs sb");
    println!("[3] btrfs {:?}  nodesize {}  used {:.2} TB",
        bsb.label, bsb.nodesize, bsb.bytes_used as f64/1e12);

    let mut map = ChunkMap::from_sys_array(&bsb.sys_chunk_array).expect("sys chunks");
    println!("    bootstrap chunks {}", map.len());
    {
        let mut tr = tree::TreeReader::new(&mut dev, &map, fs_base, bsb.nodesize);
        let items = tr.walk_leaves(bsb.chunk_root, 100_000).expect("chunk tree");
        let mut extra = ChunkMap::new();
        for it in &items {
            if it.key.key_type == mbs_reader::btrfs::CHUNK_ITEM_KEY {
                if let Ok((c, _)) = chunk::parse_chunk(&it.data, 0, it.key.offset) {
                    extra.insert(c);
                }
            }
        }
        for c in extra.chunks() { map.insert(c.clone()); }
    }
    println!("    chunks total {}", map.len());

    let mut fs = FsReader::new(&mut dev, &map, fs_base, bsb.nodesize);
    let subvols = fs.subvolumes(bsb.root).expect("subvolumes");
    println!("\n[4] subvolumes: {}", subvols.len());
    for s in &subvols {
        let flag = if s.flags & (1 << 34) != 0 { "  <-- Synology bit 34" } else { "" };
        println!("    id {:5}  {:30}  root {}  flags 0x{:x}{}",
            s.id, s.name, s.root_bytenr, s.flags, flag);
    }

    // Descend into a subvolume and list a directory.
    let Some(target) = want_subvol
        .and_then(|id| subvols.iter().find(|s| s.id == id).cloned())
        .or_else(|| subvols.iter().find(|s| s.id >= 256).cloned())
    else { return };

    println!("\n[5] listing subvol {} ({:?})", target.id, target.name);
    let mut inode = target.root_dirid;
    if let Some(p) = &want_path {
        for part in p.split('/').filter(|s| !s.is_empty()) {
            let entries = match fs.read_dir(target.root_bytenr, inode) {
                Ok(e) => e, Err(e) => { println!("    read_dir: {e}"); return; }
            };
            match entries.iter().find(|e| e.name == part) {
                Some(e) => { println!("    -> {} (inode {})", e.name, e.inode); inode = e.inode; }
                None => { println!("    {:?} not found here", part); return; }
            }
        }
    }

    match fs.read_dir(target.root_bytenr, inode) {
        Ok(entries) => {
            println!("    {} entries:", entries.len());
            for e in entries.iter().take(60) {
                let sz = fs.inode(target.root_bytenr, e.inode).ok().flatten()
                    .map(|i| format!("{:>12}", i.size)).unwrap_or_else(|| "           -".into());
                println!("      {:?} {} {:?}", e.entry_type, sz, e.name);
            }
            if entries.len() > 60 { println!("      ... and {} more", entries.len()-60); }
        }
        Err(e) => println!("    read_dir: {e}"),
    }
}
