use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

const MAX_EVIDENCE_BYTES: u64 = 16 * 1024 * 1024;

fn main() {
    if let Err(code) = run() {
        eprintln!("{code}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), &'static str> {
    let mut arguments = std::env::args_os();
    let _executable = arguments.next();
    let path = arguments.next().ok_or("RELEASE_BLAKE3_INPUT_INVALID")?;
    if arguments.next().is_some() {
        return Err("RELEASE_BLAKE3_INPUT_INVALID");
    }
    let digest =
        hash_evidence_file(Path::new(&path)).map_err(|_| "RELEASE_BLAKE3_INPUT_INVALID")?;
    println!("{digest}");
    Ok(())
}

fn hash_evidence_file(path: &Path) -> io::Result<String> {
    let link_metadata = std::fs::symlink_metadata(path)?;
    if !link_metadata.file_type().is_file()
        || link_metadata.len() == 0
        || link_metadata.len() > MAX_EVIDENCE_BYTES
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "evidence file is not a bounded regular file",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if link_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "evidence file is a reparse point",
            ));
        }
    }

    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_EVIDENCE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "opened evidence file is invalid",
        ));
    }
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > MAX_EVIDENCE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "evidence file exceeded the bound while reading",
            ));
        }
        hasher.update(&buffer[..read]);
    }
    if total == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "evidence file is empty",
        ));
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_a_bounded_regular_file_with_blake3() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("evidence.json");
        std::fs::write(&path, b"abc").expect("write evidence");
        assert_eq!(
            hash_evidence_file(&path).expect("hash evidence"),
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
        );
    }

    #[test]
    fn rejects_empty_evidence() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("empty.json");
        std::fs::write(&path, []).expect("write empty evidence");
        assert!(hash_evidence_file(&path).is_err());
    }
}
