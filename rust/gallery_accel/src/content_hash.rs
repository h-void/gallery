use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::Context;


/// Stream a file through BLAKE3 in 1 MiB chunks, mirroring
/// `app/fingerprint.py:hash_file`. Returns the hex digest.
pub fn hash_file(path: &Path, chunk_size: usize) -> anyhow::Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("open file for hashing: {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; chunk_size.max(1)];
    loop {
        let read = file
            .read(&mut buf)
            .with_context(|| format!("read file for hashing: {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}
