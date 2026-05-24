//! Content fingerprints.
//!
//! The reconciler compares local files to remote files via `(size, md5)`. Drive
//! returns `md5Checksum` for every regular file but **not** for native Google Docs;
//! those are filtered out earlier so [`from_remote`] returning `None` for them is
//! treated as "skip — out of MVP scope".
//!
//! [`compute_local`] streams the file from disk so we never load >64 KiB into RAM —
//! important for the 100 MB test bound and for later use on bigger files.

use std::path::Path;

use md5::{Digest, Md5};
use tokio::io::AsyncReadExt;

use crate::engine::RemoteFile;
use crate::error::Result;

/// Buffer size used when streaming a local file through the MD5 hasher.
const READ_BUF: usize = 64 * 1024;

/// `(size_in_bytes, hex_lowercase_md5)` for a local file. Streamed via `tokio::fs`.
pub async fn compute_local(path: &Path) -> Result<(i64, String)> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Md5::new();
    let mut buf = vec![0u8; READ_BUF];
    let mut size: i64 = 0;
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as i64;
    }
    Ok((size, hex::encode(hasher.finalize())))
}

/// `(size, md5)` extracted from a [`RemoteFile`]. Returns `None` if the remote didn't
/// expose an MD5 (e.g. native Google Docs) — caller should skip such files.
pub fn from_remote(file: &RemoteFile) -> Option<(i64, String)> {
    file.md5.as_ref().map(|md5| (file.size, md5.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn compute_local_matches_known_md5() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("f");
        std::fs::write(&p, b"hello").unwrap();
        let (size, md5) = compute_local(&p).await.unwrap();
        assert_eq!(size, 5);
        // Reference: md5("hello") = 5d41402abc4b2a76b9719d911017c592
        assert_eq!(md5, "5d41402abc4b2a76b9719d911017c592");
    }

    #[tokio::test]
    async fn compute_local_handles_files_larger_than_buf() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("big");
        // 3× the read buffer ensures we exercise the loop branch.
        let mut payload = Vec::with_capacity(READ_BUF * 3);
        for i in 0..(READ_BUF * 3) {
            payload.push((i % 256) as u8);
        }
        std::fs::write(&p, &payload).unwrap();
        let (size, md5) = compute_local(&p).await.unwrap();
        assert_eq!(size as usize, payload.len());

        // Reference via the same crate, computed in one shot.
        let expected = hex::encode({
            let mut h = Md5::new();
            h.update(&payload);
            h.finalize()
        });
        assert_eq!(md5, expected);
    }

    #[test]
    fn from_remote_propagates_md5_and_size() {
        let r = RemoteFile {
            id: "x".into(),
            mime: "text/plain".into(),
            size: 11,
            md5: Some("abc".into()),
        };
        assert_eq!(from_remote(&r), Some((11, "abc".to_owned())));
    }

    #[test]
    fn from_remote_returns_none_when_md5_missing() {
        let r = RemoteFile {
            id: "x".into(),
            mime: "application/vnd.google-apps.document".into(),
            size: 0,
            md5: None,
        };
        assert_eq!(from_remote(&r), None);
    }
}
