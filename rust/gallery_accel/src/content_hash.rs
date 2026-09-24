use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::Context;

#[cfg(test)]
const DEFAULT_CHUNK_SIZE: usize = 1024 * 1024;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blake3_matches_reference_empty_vector() {
        // Official BLAKE3 test vector for the empty input.
        let digest = blake3::hash(b"").to_hex().to_string();
        assert_eq!(
            digest,
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn hash_file_is_stable_and_matches_inline_hasher() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.bin");
        let data: Vec<u8> = (0..20_000).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();

        let from_file = hash_file(&path, DEFAULT_CHUNK_SIZE).unwrap();
        let inline = blake3::Hasher::new()
            .update(&data)
            .finalize()
            .to_hex()
            .to_string();
        assert_eq!(from_file, inline);
        // Re-hashing yields the same digest (idempotent).
        assert_eq!(from_file, hash_file(&path, 4096).unwrap());
    }
}
