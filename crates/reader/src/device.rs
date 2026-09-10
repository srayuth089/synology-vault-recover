//! Read-only block access.
//!
//! [`ReadOnlyDevice`] is the only way the rest of the crate reaches a disk, and
//! it holds a `File` opened `O_RDONLY`. It implements no write method, so a
//! stray `write_all` elsewhere cannot compile against it.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("cannot open {path} read-only: {source}")]
    Open { path: PathBuf, source: std::io::Error },
    #[error("cannot read {len} bytes at offset {offset} of {path}: {source}")]
    Read { path: PathBuf, offset: u64, len: usize, source: std::io::Error },
    #[error("{path} is not a readable block device or image file")]
    NotADevice { path: PathBuf },
}

/// A block device or image file opened for reading only.
pub struct ReadOnlyDevice {
    file: File,
    path: PathBuf,
    /// Sector size to align raw-device reads to. macOS raw devices (`/dev/rdiskN`)
    /// reject unaligned reads with EINVAL, so every read is widened to a
    /// multiple of this and then trimmed.
    sector: usize,
}

impl ReadOnlyDevice {
    /// Open a device or image read-only.
    ///
    /// On macOS prefer the raw node (`/dev/rdisk5s5`): the buffered node is far
    /// slower for the large sequential reads a filesystem walk performs.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DeviceError> {
        let path = path.as_ref().to_path_buf();
        // No OpenOptions::write anywhere: read(true) is the whole permission set.
        let file = File::options()
            .read(true)
            .open(&path)
            .map_err(|source| DeviceError::Open { path: path.clone(), source })?;
        Ok(Self { file, path, sector: 4096 })
    }

    /// Open a device, asking macOS for authorization if it is not readable.
    ///
    /// Raw disk nodes belong to `root:operator`, so a normal user needs
    /// elevation. Rather than telling the user to run `chmod`, this shows the
    /// system's own password dialog and receives an already-open read-only
    /// descriptor. The permission granted is `sys.openfile.readonly`, so the
    /// descriptor cannot be written to even by mistake.
    #[cfg(target_os = "macos")]
    pub fn open_with_authorization(path: impl AsRef<Path>) -> Result<Self, DeviceError> {
        use std::os::unix::io::FromRawFd;
        let path = path.as_ref().to_path_buf();

        // Try the ordinary path first: no prompt when we can already read it.
        if let Ok(dev) = Self::open(&path) {
            return Ok(dev);
        }

        let fd = crate::authopen::open_readonly(&path).map_err(|e| DeviceError::Open {
            path: path.clone(),
            source: std::io::Error::other(e.to_string()),
        })?;
        // SAFETY: authopen returned an owned descriptor and we take ownership.
        let file = unsafe { File::from_raw_fd(fd) };
        Ok(Self { file, path, sector: 4096 })
    }

    /// Non-macOS builds have no authorization dialog to show.
    #[cfg(not(target_os = "macos"))]
    pub fn open_with_authorization(path: impl AsRef<Path>) -> Result<Self, DeviceError> {
        Self::open(path)
    }

    /// Override the alignment used for raw reads.
    pub fn with_sector_size(mut self, sector: usize) -> Self {
        self.sector = sector.max(1);
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read exactly `len` bytes at `offset`.
    ///
    /// Alignment is handled here so callers can ask for the byte ranges the
    /// on-disk structures actually use.
    pub fn read_at(&mut self, offset: u64, len: usize) -> Result<Vec<u8>, DeviceError> {
        let sector = self.sector as u64;
        let start = offset / sector * sector;
        let skew = (offset - start) as usize;
        let span = {
            let raw = skew + len;
            // Round up to the next whole sector.
            (raw + self.sector - 1) / self.sector * self.sector
        };

        let mut buf = vec![0u8; span];
        self.file
            .seek(SeekFrom::Start(start))
            .and_then(|_| self.file.read_exact(&mut buf))
            .map_err(|source| DeviceError::Read {
                path: self.path.clone(),
                offset,
                len,
                source,
            })?;

        buf.drain(..skew);
        buf.truncate(len);
        Ok(buf)
    }

    /// Total size in bytes, for sanity checks against RAID/LVM metadata.
    pub fn size_bytes(&mut self) -> Result<u64, DeviceError> {
        // Seeking to the end works for image files; raw devices on macOS also
        // report their size this way.
        let end = self
            .file
            .seek(SeekFrom::End(0))
            .map_err(|source| DeviceError::Read {
                path: self.path.clone(),
                offset: 0,
                len: 0,
                source,
            })?;
        if end == 0 {
            return Err(DeviceError::NotADevice { path: self.path.clone() });
        }
        Ok(end)
    }
}

/// Little-endian integer helpers. Every structure this crate parses is LE,
/// including on big-endian hosts, so these are used rather than native reads.
pub(crate) fn le_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
pub(crate) fn le_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
pub(crate) fn le_u64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes([
        b[at], b[at + 1], b[at + 2], b[at + 3], b[at + 4], b[at + 5], b[at + 6], b[at + 7],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_image(bytes: &[u8]) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("mbs-dev-{}.img", std::process::id()));
        let mut f = File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        p
    }

    #[test]
    fn reads_are_realigned_around_the_requested_range() {
        // 8 KiB of recognisable bytes.
        let data: Vec<u8> = (0..8192u32).map(|i| (i % 251) as u8).collect();
        let path = temp_image(&data);
        let mut dev = ReadOnlyDevice::open(&path).unwrap().with_sector_size(4096);

        // An unaligned offset and length: the device layer must still return
        // exactly the bytes asked for.
        let got = dev.read_at(4097, 5).unwrap();
        assert_eq!(got, &data[4097..4102]);

        // A read that straddles a sector boundary.
        let got = dev.read_at(4090, 12).unwrap();
        assert_eq!(got, &data[4090..4102]);

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn little_endian_helpers_do_not_depend_on_host_order() {
        let b = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        assert_eq!(le_u16(&b, 0), 0x0201);
        assert_eq!(le_u32(&b, 0), 0x04030201);
        assert_eq!(le_u64(&b, 0), 0x0807060504030201);
    }
}
