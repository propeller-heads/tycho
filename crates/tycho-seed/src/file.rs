//! Seed files on disk: a [`Header`] followed by the package's body.

use std::{fs, path::Path};

use anyhow::{Context, Result};
use tycho_seed_format::{decode, decode_for, encode, Header};

/// Writes `body` under `header` and returns the file size.
pub fn write(path: &Path, header: &Header, body: &[u8]) -> Result<usize> {
    let bytes = encode(header, body)?;
    fs::write(path, &bytes).with_context(|| format!("writing seed to {}", path.display()))?;

    Ok(bytes.len())
}

pub fn read(path: &Path) -> Result<(Header, Vec<u8>)> {
    let bytes = read_bytes(path)?;
    let (header, body) = decode(&bytes).with_context(|| format!("{}", path.display()))?;

    Ok((header, body.to_vec()))
}

/// [`read`] for a known package, rejecting seeds written for another one.
pub fn read_for(path: &Path, package: &str) -> Result<(Header, Vec<u8>)> {
    let bytes = read_bytes(path)?;
    let (header, body) =
        decode_for(&bytes, package).with_context(|| format!("{}", path.display()))?;

    Ok((header, body.to_vec()))
}

fn read_bytes(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).with_context(|| format!("reading seed from {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Header {
        Header { package: "ethereum-example".to_owned(), block_number: 42, block_hash: [7; 32] }
    }

    #[test]
    fn round_trips_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seed.bin");

        let size = write(&path, &header(), b"body").unwrap();
        let (decoded, body) = read(&path).unwrap();

        assert_eq!(size, fs::metadata(&path).unwrap().len() as usize);
        assert_eq!(decoded, header());
        assert_eq!(body, b"body");
        assert!(read_for(&path, "ethereum-example").is_ok());
        assert!(read_for(&path, "ethereum-other")
            .unwrap_err()
            .to_string()
            .contains("seed.bin"));
    }
}
