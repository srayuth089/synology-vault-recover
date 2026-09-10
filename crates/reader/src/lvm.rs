//! LVM2 on-disk reader.
//!
//! Layout follows lvm2's `lib/label/label.h` and `lib/format_text/layout.h`:
//! a `LABELONE` label in one of the first four sectors points at a PV header,
//! which lists data areas and metadata areas; the metadata area holds the
//! volume group description as *text*, which is what we parse for the LV's
//! extent map.

use crate::device::{le_u32, le_u64, ReadOnlyDevice, DeviceError};

/// `LABEL_ID` in lvm2.
pub const LABEL_ID: &[u8; 8] = b"LABELONE";
/// `LVM2_LABEL`.
pub const LVM2_TYPE: &[u8; 8] = b"LVM2 001";
/// `FMTT_MAGIC` — marks the start of a metadata area header.
pub const FMTT_MAGIC: &[u8; 16] = b" LVM2 x[5A%r0N*>";
/// lvm2 scans the first `LABEL_SCAN_SECTORS` sectors for the label.
const LABEL_SCAN_SECTORS: u64 = 4;
const SECTOR: u64 = 512;
/// `ID_LEN`.
const ID_LEN: usize = 32;

/// A physical volume's label and the areas it points to.
#[derive(Debug, Clone)]
pub struct PvLabel {
    /// Which sector the label was found in (0..4).
    pub label_sector: u64,
    /// PV UUID, 32 raw characters as stored.
    pub pv_uuid: String,
    pub device_size_bytes: u64,
    /// Data areas: where the LV extents actually live.
    pub data_areas: Vec<DiskLocn>,
    /// Metadata areas: where the VG text description lives.
    pub metadata_areas: Vec<DiskLocn>,
}

/// `struct disk_locn` — an offset/size pair, both in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskLocn {
    pub offset: u64,
    pub size: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum LvmError {
    #[error(transparent)]
    Device(#[from] DeviceError),
    #[error("no LVM label found in the first {0} sectors")]
    NoLabel(u64),
    #[error("unsupported LVM label type {0:?}")]
    UnsupportedType(String),
    #[error("metadata area header magic not found at offset {0}")]
    NoMetadataHeader(u64),
    #[error("volume group metadata is not valid UTF-8")]
    MetadataNotText,
    #[error("logical volume {0:?} not found in volume group metadata")]
    LvNotFound(String),
    #[error("malformed volume group metadata: {0}")]
    Malformed(String),
}

/// Find and parse the `LABELONE` label at the start of a PV.
///
/// `base` is the byte offset of the PV within the device — for a Synology disk
/// this is the RAID data offset, since LVM sits inside the array.
pub fn read_label(dev: &mut ReadOnlyDevice, base: u64) -> Result<PvLabel, LvmError> {
    let scan = dev.read_at(base, (LABEL_SCAN_SECTORS * SECTOR) as usize)?;

    for sector in 0..LABEL_SCAN_SECTORS {
        let at = (sector * SECTOR) as usize;
        if &scan[at..at + 8] != LABEL_ID {
            continue;
        }
        let ty = &scan[at + 24..at + 32];
        if ty != LVM2_TYPE {
            return Err(LvmError::UnsupportedType(
                String::from_utf8_lossy(ty).trim_end().to_string(),
            ));
        }
        // offset_xl is relative to the start of the label header.
        let contents_at = at + le_u32(&scan, at + 20) as usize;
        return parse_pv_header(&scan, contents_at, sector);
    }
    Err(LvmError::NoLabel(LABEL_SCAN_SECTORS))
}

fn parse_pv_header(b: &[u8], at: usize, label_sector: u64) -> Result<PvLabel, LvmError> {
    let pv_uuid = String::from_utf8_lossy(&b[at..at + ID_LEN]).into_owned();
    let device_size_bytes = le_u64(b, at + ID_LEN);

    // Two NUL-terminated lists of disk_locn follow: data areas, then metadata
    // areas. A zeroed entry terminates each list.
    let mut cursor = at + ID_LEN + 8;
    let read_list = |cursor: &mut usize| {
        let mut out = Vec::new();
        loop {
            let offset = le_u64(b, *cursor);
            let size = le_u64(b, *cursor + 8);
            *cursor += 16;
            if offset == 0 && size == 0 {
                break;
            }
            out.push(DiskLocn { offset, size });
        }
        out
    };
    let data_areas = read_list(&mut cursor);
    let metadata_areas = read_list(&mut cursor);

    Ok(PvLabel { label_sector, pv_uuid, device_size_bytes, data_areas, metadata_areas })
}

/// Read the volume group's text metadata from a metadata area.
pub fn read_vg_metadata(
    dev: &mut ReadOnlyDevice,
    base: u64,
    mda: DiskLocn,
) -> Result<String, LvmError> {
    let header_at = base + mda.offset;
    let header = dev.read_at(header_at, 512)?;

    // checksum_xl(4) then magic(16).
    if &header[4..20] != FMTT_MAGIC {
        return Err(LvmError::NoMetadataHeader(header_at));
    }

    // raw_locns[] begins after checksum(4) + magic(16) + version(4) +
    // start(8) + size(8) = 40 bytes. The first entry points at the current
    // copy of the VG text.
    let text_offset = le_u64(&header, 40);
    let text_size = le_u64(&header, 48);
    if text_size == 0 {
        return Err(LvmError::Malformed("metadata area has zero-length text".into()));
    }

    // The text region wraps within the metadata area, so a read can straddle
    // the end and continue at the header's own size boundary.
    let area_start = header_at;
    let read_from = area_start + text_offset;
    let first = (mda.size - text_offset).min(text_size);
    let mut bytes = dev.read_at(read_from, first as usize)?;
    if first < text_size {
        // Wrapped: the remainder sits just after the 512-byte header.
        let rest = text_size - first;
        let mut tail = dev.read_at(area_start + 512, rest as usize)?;
        bytes.append(&mut tail);
    }

    String::from_utf8(bytes).map_err(|_| LvmError::MetadataNotText)
}

/// One contiguous run of a logical volume, mapped onto the physical volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// First extent of the LV this segment covers.
    pub start_extent: u64,
    pub extent_count: u64,
    /// Offset into the PV's data area, in extents.
    pub pv_start_extent: u64,
}

/// The pieces of the VG description this reader needs.
#[derive(Debug, Clone)]
pub struct VolumeGroup {
    pub name: String,
    pub extent_size_bytes: u64,
    pub logical_volumes: Vec<LogicalVolume>,
    /// Byte offset from the start of the PV to its first extent.
    ///
    /// The VG text is authoritative here; the PV header's data area should
    /// agree, but Synology volumes have been seen where only this matches.
    pub pe_start_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct LogicalVolume {
    pub name: String,
    pub segments: Vec<Segment>,
}

impl VolumeGroup {
    pub fn find_lv(&self, name: &str) -> Option<&LogicalVolume> {
        self.logical_volumes.iter().find(|lv| lv.name == name)
    }
}

impl LogicalVolume {
    /// Total size in bytes, given the VG's extent size.
    pub fn size_bytes(&self, extent_size_bytes: u64) -> u64 {
        self.segments.iter().map(|s| s.extent_count).sum::<u64>() * extent_size_bytes
    }

    /// Translate an offset within the LV to an offset within the PV data area.
    ///
    /// Returns the physical offset and how many bytes remain contiguous from
    /// there, so a caller can read across segment boundaries correctly.
    pub fn map_offset(&self, lv_offset: u64, extent_size: u64) -> Option<(u64, u64)> {
        let extent = lv_offset / extent_size;
        let within = lv_offset % extent_size;
        for seg in &self.segments {
            let end = seg.start_extent + seg.extent_count;
            if extent >= seg.start_extent && extent < end {
                let into_seg = extent - seg.start_extent;
                let phys = (seg.pv_start_extent + into_seg) * extent_size + within;
                let contiguous = (seg.extent_count - into_seg) * extent_size - within;
                return Some((phys, contiguous));
            }
        }
        None
    }
}

/// Parse the VG text description.
///
/// The format is lvm2's own config text: nested `key { ... }` blocks with
/// `name = value` entries. Only the fields needed to locate an LV's data are
/// interpreted; everything else is skipped.
pub fn parse_vg_metadata(text: &str) -> Result<VolumeGroup, LvmError> {
    let name = text
        .lines()
        .find(|l| l.contains('{') && !l.trim_start().starts_with('#'))
        .and_then(|l| l.split_whitespace().next())
        .ok_or_else(|| LvmError::Malformed("no volume group block".into()))?
        .to_string();

    let extent_size_sectors = find_number(text, "extent_size")
        .ok_or_else(|| LvmError::Malformed("no extent_size".into()))?;
    let extent_size_bytes = extent_size_sectors * SECTOR;

    let logical_volumes = parse_logical_volumes(text)?;
    // pe_start is recorded in 512-byte sectors inside physical_volumes { }.
    let pe_start_bytes = section(text, "physical_volumes")
        .and_then(|pv| find_number(&pv, "pe_start"))
        .map(|s| s * SECTOR)
        .unwrap_or(0);
    Ok(VolumeGroup { name, extent_size_bytes, logical_volumes, pe_start_bytes })
}

fn find_number(text: &str, key: &str) -> Option<u64> {
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix(key) {
            let rest = rest.trim_start();
            if let Some(v) = rest.strip_prefix('=') {
                return v.trim().split_whitespace().next()?.parse().ok();
            }
        }
    }
    None
}

fn parse_logical_volumes(text: &str) -> Result<Vec<LogicalVolume>, LvmError> {
    let mut out = Vec::new();
    let Some(lv_block) = section(text, "logical_volumes") else {
        return Ok(out);
    };

    // Each LV is a named block inside `logical_volumes { ... }`.
    let mut rest: &str = &lv_block;
    while let Some((name, body, tail)) = next_block(rest) {
        out.push(LogicalVolume { name, segments: parse_segments(&body) });
        rest = tail;
    }
    Ok(out)
}

fn parse_segments(lv_body: &str) -> Vec<Segment> {
    let mut segs = Vec::new();
    let mut rest = lv_body;
    while let Some((name, body, tail)) = next_block(rest) {
        rest = tail;
        if !name.starts_with("segment") {
            continue;
        }
        let start_extent = find_number(&body, "start_extent").unwrap_or(0);
        let extent_count = find_number(&body, "extent_count").unwrap_or(0);
        // `stripes = [ "pv0", 1234 ]` — the number is the PV start extent.
        let pv_start_extent = body
            .lines()
            .skip_while(|l| !l.trim().starts_with("stripes"))
            .take(3)
            .find_map(|l| {
                l.rsplit(',')
                    .next()
                    .and_then(|s| s.trim().trim_end_matches(']').trim().parse::<u64>().ok())
            })
            .unwrap_or(0);
        segs.push(Segment { start_extent, extent_count, pv_start_extent });
    }
    segs.sort_by_key(|s| s.start_extent);
    segs
}

/// Extract the body of `name { ... }`, balancing braces.
fn section<'a>(text: &'a str, name: &str) -> Option<String> {
    let at = text.find(name)?;
    let open = text[at..].find('{')? + at;
    let body = balanced(&text[open..])?;
    Some(body.to_string())
}

/// Given text starting at `{`, return the contents up to the matching `}`.
fn balanced(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    for (i, &c) in bytes.iter().enumerate() {
        match c {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[1..i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Find the next `name { ... }` block, returning its name, body and the rest.
fn next_block(s: &str) -> Option<(String, String, &str)> {
    let open = s.find('{')?;
    let name = s[..open].trim().lines().last()?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let body = balanced(&s[open..])?;
    let consumed = open + body.len() + 2;
    Some((name, body.to_string(), &s[consumed..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A VG description in the shape lvm2 writes, trimmed to the fields used.
    const VG_TEXT: &str = r#"
vg1000 {
id = "abcdef-0123"
seqno = 3
extent_size = 8192
max_lv = 0

physical_volumes {
pv0 {
id = "pv-uuid"
dev_size = 7813971968
pe_start = 2048
pe_count = 953853
}
}

logical_volumes {
lv {
id = "lv-uuid"
segment_count = 1

segment1 {
start_extent = 0
extent_count = 953853
type = "striped"
stripe_count = 1
stripes = [
"pv0", 0
]
}
}
}
}
"#;

    #[test]
    fn reads_the_volume_group_and_its_extent_size() {
        let vg = parse_vg_metadata(VG_TEXT).unwrap();
        assert_eq!(vg.name, "vg1000");
        // extent_size is in 512-byte sectors: 8192 * 512 = 4 MiB.
        assert_eq!(vg.extent_size_bytes, 4 * 1024 * 1024);
    }

    #[test]
    fn reads_where_the_first_extent_starts() {
        // Without pe_start the filesystem is looked for at the wrong offset,
        // which reads as "no btrfs superblock, magic was 0".
        let vg = parse_vg_metadata(VG_TEXT).unwrap();
        assert_eq!(vg.pe_start_bytes, 2048 * 512);
    }

    #[test]
    fn a_volume_group_without_pe_start_reports_zero_rather_than_guessing() {
        let text = VG_TEXT.replace("pe_start = 2048", "");
        assert_eq!(parse_vg_metadata(&text).unwrap().pe_start_bytes, 0);
    }

    #[test]
    fn finds_the_logical_volume_and_its_segment() {
        let vg = parse_vg_metadata(VG_TEXT).unwrap();
        let lv = vg.find_lv("lv").expect("vg1000/lv should be present");
        assert_eq!(lv.segments.len(), 1);
        assert_eq!(
            lv.segments[0],
            Segment { start_extent: 0, extent_count: 953853, pv_start_extent: 0 }
        );
    }

    #[test]
    fn maps_a_logical_offset_onto_the_physical_volume() {
        let vg = parse_vg_metadata(VG_TEXT).unwrap();
        let lv = vg.find_lv("lv").unwrap();
        let es = vg.extent_size_bytes;

        // Start of the LV maps to the start of the segment's PV extents.
        assert_eq!(lv.map_offset(0, es), Some((0, 953853 * es)));

        // An offset inside the first extent keeps its remainder.
        assert_eq!(lv.map_offset(1000, es), Some((1000, 953853 * es - 1000)));

        // Past the end of the LV there is no mapping, rather than a wrong one.
        assert_eq!(lv.map_offset(953853 * es, es), None);
    }

    #[test]
    fn a_multi_segment_lv_maps_across_the_boundary() {
        let text = VG_TEXT.replace(
            r#"segment1 {
start_extent = 0
extent_count = 953853
type = "striped"
stripe_count = 1
stripes = [
"pv0", 0
]
}"#,
            r#"segment1 {
start_extent = 0
extent_count = 100
stripes = [
"pv0", 500
]
}
segment2 {
start_extent = 100
extent_count = 200
stripes = [
"pv0", 900
]
}"#,
        );
        let vg = parse_vg_metadata(&text).unwrap();
        let lv = vg.find_lv("lv").unwrap();
        let es = vg.extent_size_bytes;
        assert_eq!(lv.segments.len(), 2);

        // Last byte of segment 1 stays in segment 1.
        let (phys, run) = lv.map_offset(100 * es - 1, es).unwrap();
        assert_eq!(phys, 600 * es - 1);
        assert_eq!(run, 1, "only one byte remains contiguous before the seam");

        // First byte of segment 2 jumps to its own PV location.
        let (phys, run) = lv.map_offset(100 * es, es).unwrap();
        assert_eq!(phys, 900 * es);
        assert_eq!(run, 200 * es);
    }

    #[test]
    fn the_label_magic_matches_the_published_constants() {
        assert_eq!(LABEL_ID, b"LABELONE");
        assert_eq!(LVM2_TYPE, b"LVM2 001");
        // FMTT_MAGIC written as octal escapes in lvm2's layout.h.
        assert_eq!(FMTT_MAGIC.len(), 16);
        assert_eq!(FMTT_MAGIC[0], 0o040);
        assert_eq!(FMTT_MAGIC[1], 0o114);
    }

    #[test]
    fn a_missing_lv_is_reported_rather_than_guessed() {
        let vg = parse_vg_metadata(VG_TEXT).unwrap();
        assert!(vg.find_lv("does-not-exist").is_none());
    }
}
