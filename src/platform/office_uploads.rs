//! Bounded legacy Office identification. CFB is also used by MSI and arbitrary
//! OLE objects: a valid container alone never earns an Office MIME type.

use std::io::{self, Cursor, Read, Seek, SeekFrom};

use super::{UploadKind, MAX_UPLOAD_BYTES};

pub(super) const CFB_MAGIC: &[u8; 8] = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1";

// Limit metadata work independently of the upload size. Documents are not
// extracted or decoded; only the directory and a few format-specific headers
// are read. cfb validates sector bounds, cycles and directory relationships.
struct Probe<'a> {
    cursor: Cursor<&'a [u8]>,
    remaining: usize,
}

impl Read for Probe<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Err(io::Error::other("Office metadata probe limit"));
        }
        let len = buf.len().min(self.remaining);
        let read = self.cursor.read(&mut buf[..len])?;
        self.remaining -= read;
        Ok(read)
    }
}

impl Seek for Probe<'_> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.cursor.seek(from)
    }
}

type Compound<'a> = cfb::CompoundFile<Probe<'a>>;

fn prefix<const N: usize>(file: &mut Compound<'_>, name: &str) -> Option<[u8; N]> {
    let mut header = [0; N];
    file.open_stream(name).ok()?.read_exact(&mut header).ok()?;
    Some(header)
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

pub(super) fn sniff(bytes: &[u8]) -> Option<UploadKind> {
    if bytes.len() > MAX_UPLOAD_BYTES || !bytes.starts_with(CFB_MAGIC) {
        return None;
    }
    let mut file = cfb::OpenOptions::new()
        .max_buffer_size(512)
        .open_with(Probe {
            cursor: Cursor::new(bytes),
            remaining: 4 * 1024 * 1024,
        })
        .ok()?;
    // Windows Installer's database, transform and patch classes are never
    // Office documents, even with an extra stream named WordDocument.
    let root = file.root_entry();
    let (class, second, third, tail) = root.clsid().as_fields();
    if class & 0xffff0000 == 0x000c0000
        && second == 0
        && third == 0
        && tail == &[0xc0, 0, 0, 0, 0, 0, 0, 0x46]
    {
        return None;
    }
    let word = file.is_stream("/WordDocument");
    let excel = file.is_stream("/Workbook") || file.is_stream("/Book");
    let ppt = file.is_stream("/PowerPoint Document");
    if usize::from(word) + usize::from(excel) + usize::from(ppt) != 1 {
        return None;
    }
    if word {
        // [MS-DOC] FibBase: wIdent, nFib and fWhichTblStm. The header is
        // plaintext even for a password-protected legacy document.
        let fib = prefix::<32>(&mut file, "/WordDocument")?;
        let version = u16_at(&fib, 2);
        if u16_at(&fib, 0) != 0xa5ec || !(0x0065..=0x0112).contains(&version) {
            return None;
        }
        let table = if u16_at(&fib, 10) & 0x0200 == 0 {
            "/0Table"
        } else {
            "/1Table"
        };
        if version >= 0x00c1 && !file.is_stream(table) {
            return None;
        }
        return Some(UploadKind {
            extension: "doc",
            mime: "application/msword",
        });
    }
    if excel {
        // [MS-XLS] BOF: workbook globals, BIFF5/8. A stream merely named
        // Workbook without the BIFF record is not accepted.
        let name = if file.is_stream("/Workbook") {
            "/Workbook"
        } else {
            "/Book"
        };
        let bof = prefix::<8>(&mut file, name)?;
        let record_len = u16_at(&bof, 2);
        if u16_at(&bof, 0) != 0x0809
            || !matches!(u16_at(&bof, 4), 0x0500 | 0x0600)
            || u16_at(&bof, 6) != 5
            || record_len < 8
            || u64::from(record_len) + 4 > file.entry(name).ok()?.len()
        {
            return None;
        }
        return Some(UploadKind {
            extension: "xls",
            mime: "application/vnd.ms-excel",
        });
    }
    // [MS-PPT] CurrentUserAtom points at the most recent UserEditAtom. Check
    // both headers instead of assuming the document's first record type.
    let current = prefix::<28>(&mut file, "/Current User")?;
    if u16_at(&current, 0) != 0
        || u16_at(&current, 2) != 0x0ff6
        || u32_at(&current, 4) < 20
        || u64::from(u32_at(&current, 4)) + 8 > file.entry("/Current User").ok()?.len()
        || u32_at(&current, 8) != 20
        || !matches!(u32_at(&current, 12), 0xe391c05f | 0xf3d1c4df)
        || u16_at(&current, 22) != 0x03f4
    {
        return None;
    }
    let mut stream = file.open_stream("/PowerPoint Document").ok()?;
    stream
        .seek(SeekFrom::Start(u64::from(u32_at(&current, 16))))
        .ok()?;
    let mut edit = [0; 8];
    stream.read_exact(&mut edit).ok()?;
    if u16_at(&edit, 0) != 0
        || u16_at(&edit, 2) != 0x0ff5
        || u32_at(&edit, 4) < 28
        || u64::from(u32_at(&current, 16)) + 8 + u64::from(u32_at(&edit, 4)) > stream.len()
    {
        return None;
    }
    Some(UploadKind {
        extension: "ppt",
        mime: "application/vnd.ms-powerpoint",
    })
}

#[cfg(test)]
pub(crate) fn fixture(extension: &str) -> Vec<u8> {
    use std::io::Write as _;
    let mut file = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
    match extension {
        "doc" => {
            let mut fib = [0; 32];
            fib[..4].copy_from_slice(&[0xec, 0xa5, 0xc1, 0]);
            file.create_stream("/WordDocument")
                .unwrap()
                .write_all(&fib)
                .unwrap();
            file.create_stream("/0Table")
                .unwrap()
                .write_all(&[0; 16])
                .unwrap();
        }
        "xls" => {
            let mut bof = [0; 20];
            bof[..8].copy_from_slice(&[9, 8, 16, 0, 0, 6, 5, 0]);
            file.create_stream("/Workbook")
                .unwrap()
                .write_all(&bof)
                .unwrap();
        }
        "ppt" => {
            let mut current = [0; 32];
            current[2..4].copy_from_slice(&0x0ff6u16.to_le_bytes());
            current[4..8].copy_from_slice(&24u32.to_le_bytes());
            current[8..12].copy_from_slice(&20u32.to_le_bytes());
            current[12..16].copy_from_slice(&0xe391c05fu32.to_le_bytes());
            current[22..24].copy_from_slice(&0x03f4u16.to_le_bytes());
            current[24] = 3;
            current[28] = 8;
            file.create_stream("/Current User")
                .unwrap()
                .write_all(&current)
                .unwrap();
            let mut edit = [0; 36];
            edit[2..4].copy_from_slice(&0x0ff5u16.to_le_bytes());
            edit[4..8].copy_from_slice(&28u32.to_le_bytes());
            file.create_stream("/PowerPoint Document")
                .unwrap()
                .write_all(&edit)
                .unwrap();
        }
        _ => panic!("unknown test document"),
    }
    file.into_inner().into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn office_stream_headers_identify_legacy_formats() {
        for extension in ["doc", "xls", "ppt"] {
            assert_eq!(sniff(&fixture(extension)).unwrap().extension, extension);
        }
        // Word's table flag must address an existing stream.
        let mut doc = cfb::CompoundFile::open(Cursor::new(fixture("doc"))).unwrap();
        doc.remove_stream("/0Table").unwrap();
        assert!(sniff(&doc.into_inner().into_inner()).is_none());
    }

    #[test]
    fn unrelated_ole_installers_and_forged_document_streams_are_rejected() {
        let mut file = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        file.create_stream("/Package")
            .unwrap()
            .write_all(b"MZ payload")
            .unwrap();
        assert!(sniff(file.into_inner().get_ref()).is_none());
        let mut file = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        file.create_stream("/WordDocument")
            .unwrap()
            .write_all(&[0; 32])
            .unwrap();
        assert!(sniff(file.into_inner().get_ref()).is_none());
        let mut installer = cfb::CompoundFile::open(Cursor::new(fixture("doc"))).unwrap();
        installer
            .set_storage_clsid(
                "/",
                uuid::Uuid::parse_str("000c1084-0000-0000-c000-000000000046").unwrap(),
            )
            .unwrap();
        assert!(sniff(installer.into_inner().get_ref()).is_none());
        let mut ambiguous = cfb::CompoundFile::open(Cursor::new(fixture("doc"))).unwrap();
        ambiguous
            .create_stream("/Workbook")
            .unwrap()
            .write_all(&[0; 20])
            .unwrap();
        assert!(sniff(ambiguous.into_inner().get_ref()).is_none());
    }

    #[test]
    fn corrupt_sector_chains_and_truncated_headers_fail_closed() {
        let bytes = fixture("doc");
        for len in [0, 8, 511, 512, 1024] {
            assert!(sniff(&bytes[..len]).is_none());
        }
        let mut invalid = bytes.clone();
        invalid[48..52].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(sniff(&invalid).is_none());
        // Make the directory's FAT link point back at itself.
        let mut cyclic = bytes;
        let sector_size = 1usize << u16_at(&cyclic, 30);
        let directory = u32_at(&cyclic, 48) as usize;
        let fat = u32_at(&cyclic, 76) as usize;
        let link = (fat + 1) * sector_size + directory * 4;
        cyclic[link..link + 4].copy_from_slice(&(directory as u32).to_le_bytes());
        assert!(sniff(&cyclic).is_none());
    }
}
