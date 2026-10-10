//! Minimal ZIP archive writer (stored entries, no compression).
//!
//! Folder downloads stream remote files of arbitrary size into an archive,
//! so the writer is incremental: `begin_file` / `write_data` / `end_file`
//! patch the CRC and sizes back into the local header once a file is
//! complete. Only what we need is implemented: no compression (method 0),
//! no zip64, no comments/extra fields, UTF-8 names (general-purpose bit 11).

use std::io::{self, Seek, SeekFrom, Write};

const LOCAL_HEADER_SIG: u32 = 0x0403_4b50;
const CENTRAL_HEADER_SIG: u32 = 0x0201_4b50;
const EOCD_SIG: u32 = 0x0605_4b50;
/// General-purpose bit 11: the entry name is UTF-8.
const FLAG_UTF8: u16 = 1 << 11;
const VERSION: u16 = 20;
/// External attributes: regular file 0o644 (top 16 bits).
const ATTR_FILE: u32 = 0o100644 << 16;
/// External attributes: directory 0o755 | MS-DOS directory flag.
const ATTR_DIR: u32 = (0o40755 << 16) | 0x10;

const CRC_TABLE: [u32; 256] = crc_table();

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

/// Incremental CRC-32 (IEEE 802.3, polynomial 0xEDB88320).
pub struct Crc32(u32);

impl Crc32 {
    pub fn new() -> Self {
        Self(0xffff_ffff)
    }

    pub fn update(&mut self, data: &[u8]) {
        let mut crc = self.0;
        for &byte in data {
            crc = CRC_TABLE[((crc ^ byte as u32) & 0xff) as usize] ^ (crc >> 8);
        }
        self.0 = crc;
    }

    pub fn finalize(self) -> u32 {
        self.0 ^ 0xffff_ffff
    }
}

/// One finished entry in the central directory.
struct CentralRecord {
    name: String,
    crc32: u32,
    size: u64,
    local_header_offset: u64,
    is_dir: bool,
}

struct OpenFile {
    name: String,
    local_header_offset: u64,
    crc32: Crc32,
    size: u64,
}

/// Streaming writer for a stored (uncompressed) ZIP archive.
pub struct ZipWriter<W: Write + Seek> {
    inner: W,
    central: Vec<CentralRecord>,
    open: Option<OpenFile>,
}

impl<W: Write + Seek> ZipWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            central: Vec::new(),
            open: None,
        }
    }

    /// A file entry with the given archive name (relative, `/`-separated,
    /// no leading slash, no `..` — see [`sanitize_name`]).
    pub fn begin_file(&mut self, name: &str) -> io::Result<()> {
        if self.open.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "previous entry not finished",
            ));
        }
        if name.as_bytes().len() > u16::MAX as usize {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "name too long"));
        }
        let local_header_offset = self.inner.stream_position()?;
        self.inner.write_all(&LOCAL_HEADER_SIG.to_le_bytes())?;
        self.inner.write_all(&VERSION.to_le_bytes())?;
        self.inner.write_all(&FLAG_UTF8.to_le_bytes())?;
        self.inner.write_all(&0u16.to_le_bytes())?; // method: stored
        self.inner.write_all(&0u16.to_le_bytes())?; // mod time
        self.inner.write_all(&0u16.to_le_bytes())?; // mod date
        self.inner.write_all(&0u32.to_le_bytes())?; // crc32 (patched)
        self.inner.write_all(&0u32.to_le_bytes())?; // compressed size (patched)
        self.inner.write_all(&0u32.to_le_bytes())?; // uncompressed size (patched)
        self.inner
            .write_all(&(name.as_bytes().len() as u16).to_le_bytes())?;
        self.inner.write_all(&0u16.to_le_bytes())?; // extra length
        self.inner.write_all(name.as_bytes())?;
        self.open = Some(OpenFile {
            name: name.to_string(),
            local_header_offset,
            crc32: Crc32::new(),
            size: 0,
        });
        Ok(())
    }

    pub fn write_data(&mut self, chunk: &[u8]) -> io::Result<()> {
        let Some(open) = self.open.as_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no open entry",
            ));
        };
        open.crc32.update(chunk);
        open.size += chunk.len() as u64;
        self.inner.write_all(chunk)
    }

    /// Patch the local header with the final CRC and sizes, then register
    /// the entry for the central directory.
    pub fn end_file(&mut self) -> io::Result<()> {
        let open = self.open.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "no open entry")
        })?;
        if open.size > u32::MAX as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "entry too large for zip32",
            ));
        }
        let crc32 = open.crc32.finalize();
        // Local header layout: sig(4) version(2) flags(2) method(2) time(2)
        // date(2) crc(4) csize(4) usize(4) namelen(2) extralen(2).
        let patch_at = open.local_header_offset + 4 + 2 + 2 + 2 + 2 + 2;
        let end = self.inner.stream_position()?;
        self.inner.seek(SeekFrom::Start(patch_at))?;
        self.inner.write_all(&crc32.to_le_bytes())?;
        self.inner.write_all(&(open.size as u32).to_le_bytes())?;
        self.inner.write_all(&(open.size as u32).to_le_bytes())?;
        self.inner.seek(SeekFrom::Start(end))?;
        self.central.push(CentralRecord {
            name: open.name,
            crc32,
            size: open.size,
            local_header_offset: open.local_header_offset,
            is_dir: false,
        });
        Ok(())
    }

    /// An explicit directory entry (`name` must already end in `/`), so
    /// empty folders survive the round trip.
    pub fn add_directory(&mut self, name: &str) -> io::Result<()> {
        if self.open.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "previous entry not finished",
            ));
        }
        if name.as_bytes().len() > u16::MAX as usize {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "name too long"));
        }
        let local_header_offset = self.inner.stream_position()?;
        self.inner.write_all(&LOCAL_HEADER_SIG.to_le_bytes())?;
        self.inner.write_all(&VERSION.to_le_bytes())?;
        self.inner.write_all(&FLAG_UTF8.to_le_bytes())?;
        self.inner.write_all(&0u16.to_le_bytes())?; // stored
        self.inner.write_all(&0u16.to_le_bytes())?; // time
        self.inner.write_all(&0u16.to_le_bytes())?; // date
        self.inner.write_all(&0u32.to_le_bytes())?; // crc
        self.inner.write_all(&0u32.to_le_bytes())?; // csize
        self.inner.write_all(&0u32.to_le_bytes())?; // usize
        self.inner
            .write_all(&(name.as_bytes().len() as u16).to_le_bytes())?;
        self.inner.write_all(&0u16.to_le_bytes())?;
        self.inner.write_all(name.as_bytes())?;
        self.central.push(CentralRecord {
            name: name.to_string(),
            crc32: 0,
            size: 0,
            local_header_offset,
            is_dir: true,
        });
        Ok(())
    }

    /// Write the central directory and end-of-central-directory record,
    /// then return the underlying writer.
    pub fn finish(mut self) -> io::Result<W> {
        if self.open.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "entry still open",
            ));
        }
        if self.central.len() > u16::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "too many entries for zip32",
            ));
        }
        let central_offset = self.inner.stream_position()?;
        let mut central_size = 0u64;
        for record in &self.central {
            let name_len = record.name.as_bytes().len() as u16;
            let size32 = record.size as u32;
            self.inner.write_all(&CENTRAL_HEADER_SIG.to_le_bytes())?;
            // Version made by (unix host, ZIP 2.0) + version needed.
            self.inner.write_all(&(((3u16) << 8) | VERSION).to_le_bytes())?;
            self.inner.write_all(&VERSION.to_le_bytes())?;
            self.inner.write_all(&FLAG_UTF8.to_le_bytes())?;
            self.inner.write_all(&0u16.to_le_bytes())?; // method: stored
            self.inner.write_all(&0u16.to_le_bytes())?; // mod time
            self.inner.write_all(&0u16.to_le_bytes())?; // mod date
            self.inner.write_all(&record.crc32.to_le_bytes())?;
            self.inner.write_all(&size32.to_le_bytes())?; // compressed size
            self.inner.write_all(&size32.to_le_bytes())?; // uncompressed size
            self.inner.write_all(&name_len.to_le_bytes())?;
            self.inner.write_all(&0u16.to_le_bytes())?; // extra length
            self.inner.write_all(&0u16.to_le_bytes())?; // comment length
            self.inner.write_all(&0u16.to_le_bytes())?; // disk number
            self.inner.write_all(&0u16.to_le_bytes())?; // internal attributes
            let external = if record.is_dir { ATTR_DIR } else { ATTR_FILE };
            self.inner.write_all(&external.to_le_bytes())?;
            self.inner
                .write_all(&(record.local_header_offset as u32).to_le_bytes())?;
            self.inner.write_all(record.name.as_bytes())?;
            central_size += 46 + name_len as u64;
        }
        let count = self.central.len() as u16;
        self.inner.write_all(&EOCD_SIG.to_le_bytes())?;
        self.inner.write_all(&0u16.to_le_bytes())?; // disk number
        self.inner.write_all(&0u16.to_le_bytes())?; // central dir disk
        self.inner.write_all(&count.to_le_bytes())?; // entries, this disk
        self.inner.write_all(&count.to_le_bytes())?; // entries, total
        self.inner
            .write_all(&(central_size as u32).to_le_bytes())?;
        self.inner
            .write_all(&(central_offset as u32).to_le_bytes())?;
        self.inner.write_all(&0u16.to_le_bytes())?; // comment length
        self.inner.flush()?;
        Ok(self.inner)
    }
}

/// Make a remote path safe for use as an archive member name: strip the
/// leading slash, drop `.`/`..` components, and collapse separators.
pub fn sanitize_name(name: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in name.split('/') {
        match part {
            "" | "." | ".." => {}
            part => parts.push(part),
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_vector() {
        let mut crc = Crc32::new();
        crc.update(b"123456789");
        assert_eq!(crc.finalize(), 0xcbf4_3926);
    }

    #[test]
    fn sanitize_strips_traversal() {
        assert_eq!(sanitize_name("/var/log/syslog"), Some("var/log/syslog".into()));
        assert_eq!(sanitize_name("/a/../b/./c"), Some("a/b/c".into()));
        assert_eq!(sanitize_name("/"), None);
        assert_eq!(sanitize_name(".."), None);
    }

    /// Parse the finished archive with independent little-endian readers:
    /// EOCD → central directory → local headers → stored payloads.
    // [utest->req~folder-zip-progress~1]
    #[test]
    fn archive_round_trips() {
        let mut writer = ZipWriter::new(io::Cursor::new(Vec::new()));
        writer.begin_file("hello.txt").unwrap();
        writer.write_data(b"hello world").unwrap();
        writer.end_file().unwrap();
        writer.begin_file("dir/nested.bin").unwrap();
        writer.write_data(&[1, 2, 3, 4]).unwrap();
        writer.end_file().unwrap();
        writer.add_directory("empty/").unwrap();
        let cursor = writer.finish().unwrap();
        let bytes = cursor.into_inner();

        // End of central directory: last 22 bytes, no comment.
        let eocd = &bytes[bytes.len() - 22..];
        assert_eq!(u32::from_le_bytes(eocd[0..4].try_into().unwrap()), EOCD_SIG);
        let count = u16::from_le_bytes(eocd[10..12].try_into().unwrap()) as usize;
        assert_eq!(count, 3);
        let cd_offset = u32::from_le_bytes(eocd[16..20].try_into().unwrap()) as usize;

        // Central directory: walk `count` headers and index them by name.
        let mut pos = cd_offset;
        let mut entries: Vec<(String, u32, u64, u64)> = Vec::new();
        for _ in 0..count {
            assert_eq!(
                u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()),
                CENTRAL_HEADER_SIG
            );
            let crc = u32::from_le_bytes(bytes[pos + 16..pos + 20].try_into().unwrap());
            let size = u32::from_le_bytes(bytes[pos + 24..pos + 28].try_into().unwrap()) as u64;
            let name_len = u16::from_le_bytes(bytes[pos + 28..pos + 30].try_into().unwrap()) as usize;
            let extra_len = u16::from_le_bytes(bytes[pos + 30..pos + 32].try_into().unwrap()) as usize;
            let comment_len = u16::from_le_bytes(bytes[pos + 32..pos + 34].try_into().unwrap()) as usize;
            let local_offset = u32::from_le_bytes(bytes[pos + 42..pos + 46].try_into().unwrap()) as u64;
            let name = String::from_utf8(bytes[pos + 46..pos + 46 + name_len].to_vec()).unwrap();
            entries.push((name, crc, size, local_offset));
            pos += 46 + name_len + extra_len + comment_len;
        }
        assert_eq!(pos, bytes.len() - 22, "central directory sized exactly");

        // Every local header agrees with its central record; payloads match.
        let payloads: [(&str, &[u8]); 2] =
            [("hello.txt", b"hello world"), ("dir/nested.bin", &[1, 2, 3, 4])];
        for (name, payload) in payloads {
            let (_, crc, size, local_offset) = entries
                .iter()
                .find(|(entry_name, _, _, _)| entry_name == name)
                .expect("entry present");
            let local = *local_offset as usize;
            assert_eq!(
                u32::from_le_bytes(bytes[local..local + 4].try_into().unwrap()),
                LOCAL_HEADER_SIG
            );
            assert_eq!(
                u32::from_le_bytes(bytes[local + 14..local + 18].try_into().unwrap()),
                *crc,
                "local crc matches central for {name}"
            );
            assert_eq!(
                u32::from_le_bytes(bytes[local + 22..local + 26].try_into().unwrap()),
                *size as u32
            );
            let name_len = u16::from_le_bytes(bytes[local + 26..local + 28].try_into().unwrap()) as usize;
            let extra_len = u16::from_le_bytes(bytes[local + 28..local + 30].try_into().unwrap()) as usize;
            let data_start = local + 30 + name_len + extra_len;
            assert_eq!(&bytes[data_start..data_start + *size as usize], payload);
            let mut check = Crc32::new();
            check.update(payload);
            assert_eq!(check.finalize(), *crc);
        }
        let dir = entries
            .iter()
            .find(|(name, _, _, _)| name == "empty/")
            .expect("dir entry present");
        assert_eq!(dir.2, 0, "directory has no payload");
    }
}
