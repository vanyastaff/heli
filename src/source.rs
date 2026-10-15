//! Content identity before Git's clean filters: what the compiler reads.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use sha2::{Digest, Sha256};

pub fn digest(path: &Path) -> io::Result<String> {
    digest_reader(&mut File::open(path)?)
}

pub fn digest_reader(reader: &mut impl Read) -> io::Result<String> {
    let mut hash = Sha256::new();
    let mut bytes = [0; 16 * 1024];
    loop {
        let count = reader.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        hash.update(&bytes[..count]);
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
