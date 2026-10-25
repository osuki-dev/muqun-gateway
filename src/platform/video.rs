//! Bounded container recognition shared by uploads and workspace/session assets.
//! This identifies containers, not the codecs a particular phone can decode.
//! Header references: https://mp4ra.org/registered-types/brands,
//! https://www.webmproject.org/docs/container/, and Media3's progressive extractors.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VideoContainer {
    pub(crate) extension: &'static str,
    pub(crate) mime: &'static str,
}

pub(crate) fn sniff_video_container(bytes: &[u8]) -> Option<VideoContainer> {
    // Keep work bounded even when called with an entire upload body.
    let bytes = &bytes[..bytes.len().min(8 * 1024)];
    if let Some(container) = iso_container(bytes) {
        return Some(container);
    }
    let (extension, mime) = if let Some(doctype) = ebml_doctype(bytes) {
        match doctype {
            b"webm" => ("webm", "video/webm"),
            b"matroska" => ("mkv", "video/x-matroska"),
            _ => return None,
        }
    } else if transport_stream(bytes) {
        ("ts", "video/mp2t")
    } else if program_stream(bytes) {
        ("mpeg", "video/mpeg")
    } else if flash_video(bytes) {
        ("flv", "video/x-flv")
    } else if avi(bytes) {
        ("avi", "video/x-msvideo")
    } else {
        return None;
    };
    Some(VideoContainer { extension, mime })
}

fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn iso_container(bytes: &[u8]) -> Option<VideoContainer> {
    let mut at = 0usize;
    for _ in 0..16 {
        let size = usize::try_from(be32(bytes, at)?).ok()?;
        let kind = bytes.get(at + 4..at + 8)?;
        let (size, header) = if size == 1 {
            let large = u64::from_be_bytes(bytes.get(at + 8..at + 16)?.try_into().ok()?);
            (usize::try_from(large).ok()?, 16)
        } else {
            (size, 8)
        };
        if size < header {
            return None;
        }
        let end = at.checked_add(size)?;
        if kind == b"ftyp" {
            let brand = bytes.get(at + header..at + header + 4)?;
            // A complete ftyp includes the minor version and whole compatible brands.
            if size < header + 8 || (size - header) % 4 != 0 || end > bytes.len() {
                return None;
            }
            // These positively identify audio-only or image containers, even
            // if their compatible-brand list also mentions generic ISO media.
            if matches!(
                brand,
                b"M4A "
                    | b"M4B "
                    | b"M4P "
                    | b"F4A "
                    | b"F4B "
                    | b"heic"
                    | b"heix"
                    | b"hevc"
                    | b"hevx"
                    | b"heim"
                    | b"heis"
                    | b"mif1"
                    | b"msf1"
                    | b"avif"
                    | b"avis"
                    | b"avci"
                    | b"avcs"
            ) {
                return None;
            }
            let (extension, mime) = match brand {
                b"qt  " => ("mov", "video/quicktime"),
                b"3g2a" | b"3g2b" | b"3g2c" => ("3g2", "video/3gpp2"),
                b"3gp4" | b"3gp5" | b"3gp6" | b"3gp7" | b"3gp8" | b"3gp9" | b"3ge6" | b"3ge7"
                | b"3ge9" | b"3gg6" | b"3gg9" | b"3gr6" | b"3gr9" | b"3gs6" | b"3gs9" | b"3gt8"
                | b"3gt9" => ("3gp", "video/3gpp"),
                b"isom" | b"iso2" | b"iso3" | b"iso4" | b"iso5" | b"iso6" | b"iso7" | b"iso8"
                | b"iso9" | b"mp41" | b"mp42" | b"avc1" | b"hvc1" | b"hev1" | b"av01" | b"M4V "
                | b"M4VH" | b"M4VP" | b"F4V " => ("mp4", "video/mp4"),
                _ => return None,
            };
            return Some(VideoContainer { extension, mime });
        }
        // Skip only recognized padding boxes, never search arbitrary payloads.
        if !matches!(kind, b"free" | b"skip" | b"wide") || end > bytes.len() {
            return None;
        }
        at = end;
    }
    None
}

fn ebml_integer(bytes: &[u8], at: usize, id: bool) -> Option<(u64, usize)> {
    let first = *bytes.get(at)?;
    let length = first.leading_zeros() as usize + 1;
    if length > if id { 4 } else { 8 } {
        return None;
    }
    let encoded = bytes.get(at..at.checked_add(length)?)?;
    let mut value = if id {
        first as u64
    } else {
        (first & (0xffu16 >> length) as u8) as u64
    };
    for byte in &encoded[1..] {
        value = (value << 8) | *byte as u64;
    }
    // Unknown sizes cannot delimit a header or a header element.
    if !id && value == (1u64 << (7 * length)) - 1 {
        return None;
    }
    Some((value, length))
}

fn ebml_doctype(bytes: &[u8]) -> Option<&[u8]> {
    if !bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return None;
    }
    let (size, length) = ebml_integer(bytes, 4, false)?;
    let mut at = 4 + length;
    let end = at.checked_add(usize::try_from(size).ok()?)?;
    if end > bytes.len() || end > 4096 {
        return None;
    }
    let mut doctype = None;
    while at < end {
        let (id, length) = ebml_integer(bytes, at, true)?;
        at += length;
        let (size, length) = ebml_integer(bytes, at, false)?;
        at += length;
        let next = at.checked_add(usize::try_from(size).ok()?)?;
        if next > end {
            return None;
        }
        if id == 0x4282 {
            if doctype.is_some() {
                return None;
            }
            doctype = Some(bytes.get(at..next)?);
        }
        at = next;
    }
    // An EBML header alone is not yet a Matroska/WebM segment.
    if bytes.get(end..end + 4)? != [0x18, 0x53, 0x80, 0x67] {
        return None;
    }
    let segment_size_length = bytes.get(end + 4)?.leading_zeros() as usize + 1;
    if segment_size_length > 8 || end + 4 + segment_size_length > bytes.len() {
        return None;
    }
    doctype
}

fn transport_stream(bytes: &[u8]) -> bool {
    // TS, timestamp-prefixed M2TS and Reed-Solomon-padded broadcast packets.
    [(188, 0), (192, 4), (204, 0)]
        .into_iter()
        .any(|(stride, prefix)| {
            bytes.len() >= stride * 3
                && (0..3).all(|index| {
                    let at = index * stride + prefix;
                    let header = &bytes[at..at + 4];
                    let control = (header[3] >> 4) & 3;
                    header[0] == 0x47
                        && header[1] & 0x80 == 0
                        && control != 0
                        && match control {
                            2 => bytes[at + 4] == 183,
                            3 => bytes[at + 4] <= 182,
                            _ => true,
                        }
                })
        })
}

fn program_stream(bytes: &[u8]) -> bool {
    if bytes.len() < 17 || !bytes.starts_with(&[0, 0, 1, 0xba]) {
        return false;
    }
    let next = 14 + (bytes[13] & 7) as usize;
    bytes[4] & 0xc4 == 0x44
        && bytes[6] & 4 == 4
        && bytes[8] & 4 == 4
        && bytes[9] & 1 == 1
        && bytes[12] & 3 == 3
        && bytes.get(next..next + 3) == Some(&[0, 0, 1])
}

fn flash_video(bytes: &[u8]) -> bool {
    if bytes.len() < 13 || !bytes.starts_with(b"FLV\x01") || bytes[4] & !5 != 0 || bytes[4] & 1 == 0
    {
        return false;
    }
    let Some(offset) = be32(bytes, 5).and_then(|offset| usize::try_from(offset).ok()) else {
        return false;
    };
    offset >= 9 && bytes.get(offset..offset.saturating_add(4)) == Some(&[0, 0, 0, 0])
}

fn avi(bytes: &[u8]) -> bool {
    if bytes.len() < 24 || !bytes.starts_with(b"RIFF") || &bytes[8..12] != b"AVI " {
        return false;
    }
    let riff_size = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    if riff_size < 16 {
        return false;
    }
    let mut at = 12usize;
    for _ in 0..16 {
        let Some(header) = bytes.get(at..at.saturating_add(12)) else {
            return false;
        };
        let size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        if at
            .checked_add(8)
            .and_then(|at| at.checked_add(size))
            .is_none_or(|end| end > riff_size.saturating_add(8))
        {
            return false;
        }
        if &header[..4] == b"LIST" && size >= 4 && &header[8..12] == b"hdrl" {
            return true;
        }
        if &header[..4] != b"JUNK" {
            return false;
        }
        let Some(next) = at
            .checked_add(8)
            .and_then(|at| at.checked_add(size))
            .and_then(|at| at.checked_add(size % 2))
        else {
            return false;
        };
        at = next;
    }
    false
}

#[cfg(test)]
pub(crate) fn test_video_containers() -> Vec<(&'static str, Vec<u8>, VideoContainer)> {
    let mut cases = Vec::new();
    for (label, brand, extension, mime) in [
        ("MP4", b"isom", "mp4", "video/mp4"),
        ("M4V", b"M4V ", "mp4", "video/mp4"),
        ("QuickTime", b"qt  ", "mov", "video/quicktime"),
        ("3GPP", b"3gp6", "3gp", "video/3gpp"),
        ("3GPP2", b"3g2a", "3g2", "video/3gpp2"),
    ] {
        let mut bytes = b"\x00\x00\x00\x14ftyp".to_vec();
        bytes.extend_from_slice(brand);
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(brand);
        cases.push((label, bytes, VideoContainer { extension, mime }));
    }
    for (label, doctype, extension, mime) in [
        ("WebM", "webm", "webm", "video/webm"),
        ("Matroska", "matroska", "mkv", "video/x-matroska"),
    ] {
        let mut bytes = vec![
            0x1a,
            0x45,
            0xdf,
            0xa3,
            0x80 | (3 + doctype.len()) as u8,
            0x42,
            0x82,
            0x80 | doctype.len() as u8,
        ];
        bytes.extend_from_slice(doctype.as_bytes());
        bytes.extend_from_slice(&[0x18, 0x53, 0x80, 0x67, 0xff]);
        cases.push((label, bytes, VideoContainer { extension, mime }));
    }
    for (label, stride, prefix) in [("TS", 188, 0), ("M2TS", 192, 4), ("TS204", 204, 0)] {
        let mut bytes = vec![0; stride * 3];
        for index in 0..3 {
            bytes[index * stride + prefix..index * stride + prefix + 4]
                .copy_from_slice(&[0x47, 0x40, 0, 0x10]);
        }
        cases.push((
            label,
            bytes,
            VideoContainer {
                extension: "ts",
                mime: "video/mp2t",
            },
        ));
    }
    cases.push((
        "MPEG-PS",
        vec![0, 0, 1, 0xba, 0x44, 0, 4, 0, 4, 1, 0, 0, 3, 0, 0, 0, 1],
        VideoContainer {
            extension: "mpeg",
            mime: "video/mpeg",
        },
    ));
    cases.push((
        "FLV",
        b"FLV\x01\x05\x00\x00\x00\x09\x00\x00\x00\x00".to_vec(),
        VideoContainer {
            extension: "flv",
            mime: "video/x-flv",
        },
    ));
    cases.push((
        "AVI",
        b"RIFF\x10\x00\x00\x00AVI LIST\x04\x00\x00\x00hdrl".to_vec(),
        VideoContainer {
            extension: "avi",
            mime: "video/x-msvideo",
        },
    ));
    cases
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_headers_are_shared_across_upload_stored_read_and_workspace_assets() {
        let root = crate::asset_test_dir("shared-video-containers");
        for (label, bytes, expected) in test_video_containers() {
            assert_eq!(sniff_video_container(&bytes), Some(expected), "{label}");
            let upload = crate::sniff_video_upload_kind(&bytes).unwrap();
            assert_eq!(
                (upload.extension, upload.mime),
                (expected.extension, expected.mime),
                "{label}"
            );
            let path = root.join(format!("{label}.txt"));
            std::fs::write(&path, &bytes).unwrap();
            assert_eq!(
                crate::sniff_stored_upload(&path, "misleading.txt")
                    .unwrap()
                    .mime,
                expected.mime,
                "{label}"
            );
            let workspace = crate::sniff_asset_type(&bytes, "misleading.txt");
            assert_eq!(workspace.kind, crate::AssetKind::Video, "{label}");
            assert_eq!(workspace.mime, expected.mime, "{label}");
            for end in 0..bytes.len() {
                assert_eq!(
                    sniff_video_container(&bytes[..end]),
                    None,
                    "{label} truncated at {end}"
                );
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn iso_audio_and_images_cannot_be_laundered_through_compatible_video_brands() {
        for brand in [
            b"M4A ", b"M4B ", b"F4A ", b"heic", b"mif1", b"avif", b"avis",
        ] {
            let mut bytes = b"\x00\x00\x00\x14ftyp".to_vec();
            bytes.extend_from_slice(brand);
            bytes.extend_from_slice(&[0; 4]);
            bytes.extend_from_slice(b"isom");
            assert_eq!(sniff_video_container(&bytes), None);
        }
        assert_eq!(sniff_video_container(b"recording.mp4 video/mp4"), None);
        assert_eq!(sniff_video_container(b"#EXTM3U\nrecording.mp4"), None);
    }

    #[test]
    fn malformed_container_lengths_and_embedded_doctype_strings_are_refused() {
        let mut webm = test_video_containers()
            .into_iter()
            .find(|case| case.0 == "WebM")
            .unwrap()
            .1;
        webm[5] = 0xec; // A Void element containing the same bytes is not DocType.
        assert_eq!(sniff_video_container(&webm), None);
        for length in [0, 1, 7, 8, 15, 19, u32::MAX] {
            let mut bytes = b"\x00\x00\x00\x14ftypisom\x00\x00\x00\x00isom".to_vec();
            bytes[..4].copy_from_slice(&length.to_be_bytes());
            assert_eq!(sniff_video_container(&bytes), None);
        }
        assert_eq!(
            ebml_integer(&[1, 0, 0, 0, 0, 0, 0, 1], 0, false),
            Some((1, 8))
        );
        assert_eq!(ebml_integer(&[0], 0, false), None);
        let mut ts = test_video_containers()
            .into_iter()
            .find(|case| case.0 == "TS")
            .unwrap()
            .1;
        ts[188] = 0;
        assert_eq!(sniff_video_container(&ts), None);
        assert_eq!(
            sniff_video_container(b"FLV\x01\x05\xff\xff\xff\xff\x00\x00\x00\x00"),
            None
        );
        assert_eq!(
            sniff_video_container(b"RIFF\x00\x00\x00\x00AVI LIST\x04\x00\x00\x00hdrl"),
            None
        );
    }
}
