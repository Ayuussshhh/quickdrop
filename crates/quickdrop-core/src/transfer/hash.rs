//! Streaming BLAKE3 helpers.

use std::path::Path;

use tokio::fs::File;
use tokio::io::AsyncReadExt;

use crate::Result;

/// Hash a whole file with BLAKE3 streaming. Returns hex-encoded.
pub async fn blake3_file(path: &Path) -> Result<String> {
    let mut f = File::open(path).await?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}
