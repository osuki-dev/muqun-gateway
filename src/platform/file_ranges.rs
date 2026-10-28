//! Bounded file reads. A continuation must name the same file representation.

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use sha2::{Digest, Sha256};

use crate::{api_error, ApiResult};

pub(crate) const MAX_FILE_RANGE_BYTES: u64 = 1024 * 1024;

pub(crate) struct FileRange {
    pub(crate) offset: u64,
    pub(crate) length: u64,
    total: u64,
    pub(crate) etag: String,
    version: String,
    pub(crate) partial: bool,
}

impl FileRange {
    pub(crate) fn select(metadata: &std::fs::Metadata, headers: &HeaderMap) -> ApiResult<Self> {
        let version = file_version(metadata);
        // Mutable workspace files have a fingerprint, not an immutable content
        // digest. Never advertise that fingerprint as a strong HTTP validator.
        let etag = format!("W/\"{version}\"");
        if headers
            .get("if-match")
            .is_some_and(|value| value.as_bytes() != b"*")
            || headers
                .get("x-muqun-file-version")
                .is_some_and(|value| value.as_bytes() != version.as_bytes())
        {
            return Err(api_error(
                StatusCode::PRECONDITION_FAILED,
                "file_changed",
                "the file changed; restart the download",
            ));
        }
        let total = metadata.len();
        let Some(range) = headers.get("range") else {
            return Ok(Self {
                offset: 0,
                length: total,
                total,
                etag,
                version,
                partial: false,
            });
        };
        let refused = || {
            api_error(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "invalid_file_range",
                "request one byte range of at most 1048576 bytes",
            )
        };
        let text = range.to_str().map_err(|_| refused())?;
        if text.len() > 128 || total == 0 {
            return Err(refused());
        }
        let (start, end) = text
            .strip_prefix("bytes=")
            .and_then(|text| text.split_once('-'))
            .ok_or_else(refused)?;
        let (offset, end) = if start.is_empty() {
            let count = end
                .parse::<u64>()
                .ok()
                .filter(|n| *n > 0 && *n <= MAX_FILE_RANGE_BYTES)
                .ok_or_else(refused)?;
            (total.saturating_sub(count), total - 1)
        } else {
            let offset = start.parse::<u64>().map_err(|_| refused())?;
            let end = if end.is_empty() {
                offset
                    .saturating_add(MAX_FILE_RANGE_BYTES - 1)
                    .min(total - 1)
            } else {
                end.parse::<u64>().map_err(|_| refused())?.min(total - 1)
            };
            (offset, end)
        };
        if offset >= total || end < offset || end - offset >= MAX_FILE_RANGE_BYTES {
            return Err(refused());
        }
        Ok(Self {
            offset,
            length: end - offset + 1,
            total,
            etag,
            version,
            partial: true,
        })
    }

    pub(crate) fn headers(
        &self,
        builder: axum::http::response::Builder,
    ) -> axum::http::response::Builder {
        let builder = builder
            .status(if self.partial {
                StatusCode::PARTIAL_CONTENT
            } else {
                StatusCode::OK
            })
            .header("accept-ranges", "bytes")
            .header("etag", &self.etag)
            .header("x-muqun-file-version", &self.version)
            .header("content-length", self.length);
        if self.partial {
            builder.header(
                "content-range",
                format!(
                    "bytes {}-{}/{}",
                    self.offset,
                    self.offset + self.length - 1,
                    self.total
                ),
            )
        } else {
            builder
        }
    }

    pub(crate) async fn body(&self, mut file: tokio::fs::File) -> std::io::Result<Body> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        file.seek(std::io::SeekFrom::Start(self.offset)).await?;
        let mut file = file.take(self.length);
        let expected = self.version.clone();
        let mut remaining = self.length;
        Ok(Body::from_stream(async_stream::stream! {
            let mut buffer = vec![0u8; super::assets::ASSET_CONTENT_CHUNK_BYTES];
            while remaining > 0 {
                match file.read(&mut buffer).await {
                    Ok(0) => { yield Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof)); break; },
                    Ok(count) => {
                        if file.get_ref().metadata().await.ok().is_none_or(|metadata| file_version(&metadata) != expected) {
                            yield Err(std::io::Error::other("file changed during read")); break;
                        }
                        remaining -= count as u64;
                        yield Ok::<_, std::io::Error>(axum::body::Bytes::copy_from_slice(&buffer[..count]));
                    },
                    Err(error) => { yield Err(error); break; }
                }
            }
        }))
    }
}

fn file_version(metadata: &std::fs::Metadata) -> String {
    let mut hash = Sha256::new();
    hash.update(metadata.len().to_le_bytes());
    hash.update(format!("{:?}", metadata.modified().ok()).as_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        hash.update(metadata.dev().to_le_bytes());
        hash.update(metadata.ino().to_le_bytes());
        hash.update(metadata.ctime().to_le_bytes());
        hash.update(metadata.ctime_nsec().to_le_bytes());
    }
    hex::encode(hash.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ranges_bound_large_files_and_fence_changed_representations() {
        let path = crate::test_state_dir().join(format!("file-range-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(100 * 1024 * 1024).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("range", "bytes=1048576-".parse().unwrap());
        let selected = FileRange::select(&file.metadata().unwrap(), &headers).unwrap();
        assert_eq!(
            (selected.offset, selected.length),
            (1048576, MAX_FILE_RANGE_BYTES)
        );
        let body = selected
            .body(tokio::fs::File::open(&path).await.unwrap())
            .await
            .unwrap();
        assert_eq!(
            axum::body::to_bytes(body, MAX_FILE_RANGE_BYTES as usize)
                .await
                .unwrap()
                .len(),
            MAX_FILE_RANGE_BYTES as usize
        );
        headers.insert("x-muqun-file-version", selected.version.parse().unwrap());
        file.set_len(100 * 1024 * 1024 + 1).unwrap();
        assert_eq!(
            FileRange::select(&file.metadata().unwrap(), &headers)
                .err()
                .unwrap()
                .0,
            StatusCode::PRECONDITION_FAILED
        );
        headers.remove("x-muqun-file-version");
        for invalid in [
            "bytes=0-1048576",
            "bytes=9-1",
            "bytes=0-1,3-4",
            "bytes=18446744073709551615-",
        ] {
            headers.insert("range", invalid.parse().unwrap());
            assert_eq!(
                FileRange::select(&file.metadata().unwrap(), &headers)
                    .err()
                    .unwrap()
                    .0,
                StatusCode::RANGE_NOT_SATISFIABLE
            );
        }
        std::fs::remove_file(path).unwrap();
    }
}
