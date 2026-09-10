//! A read-only reader for the Synology stack: mdadm RAID1 → LVM2 → Btrfs.
//!
//! Written from published on-disk formats so the product carries no GPL
//! obligation, and deliberately read-only: there is no code path in this crate
//! that opens a device for writing, so "never write to the source" is a
//! property of the type system rather than a rule someone has to remember.

pub mod device;
pub mod authopen;
pub mod btrfs;
pub mod lvm;
pub mod mdadm;

pub use device::{ReadOnlyDevice, DeviceError};

/// Read a length-prefixed name out of a `ROOT_REF`/`DIR_ITEM` payload.
///
/// Exposed for the probe example; the higher-level API returns names directly.
pub fn device_str(data: &[u8]) -> String {
    // root_ref: dirid(8) sequence(8) name_len(2) then the name.
    if data.len() < 18 { return String::new(); }
    let n = u16::from_le_bytes([data[16], data[17]]) as usize;
    let end = (18 + n).min(data.len());
    String::from_utf8_lossy(&data[18..end]).into_owned()
}
