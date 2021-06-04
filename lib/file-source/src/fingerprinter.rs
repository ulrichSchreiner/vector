use crate::{metadata_ext::PortableFileExt, FileSourceInternalEvents};
use crc::Crc;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, metadata, File},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

const FINGERPRINT_CRC: Crc<u64> = Crc::<u64>::new(&crc::CRC_64_ECMA_182);

#[derive(Clone)]
pub struct Fingerprinter {
    pub strategy: FingerprintStrategy,
    pub max_line_length: usize,
    pub ignore_not_found: bool,
}

#[derive(Clone)]
pub enum FingerprintStrategy {
    Checksum {
        bytes: usize,
        ignored_header_bytes: usize,
    },
    FirstLineChecksum {
        ignored_header_bytes: usize,
    },
    DevInode,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileFingerprint {
    #[serde(rename = "checksum")]
    BytesChecksum(u64),
    FirstLineChecksum(u64),
    DevInode(u64, u64),
    Unknown(u64),
}

impl FileFingerprint {
    pub fn to_legacy(&self) -> u64 {
        use FileFingerprint::*;

        match self {
            BytesChecksum(c) => *c,
            FirstLineChecksum(c) => *c,
            DevInode(dev, ino) => {
                let mut buf = Vec::with_capacity(std::mem::size_of_val(dev) * 2);
                buf.write_all(&dev.to_be_bytes()).expect("writing to array");
                buf.write_all(&ino.to_be_bytes()).expect("writing to array");
                FINGERPRINT_CRC.checksum(&buf[..])
            }
            Unknown(c) => *c,
        }
    }
}

impl From<u64> for FileFingerprint {
    fn from(c: u64) -> Self {
        FileFingerprint::Unknown(c)
    }
}

impl Fingerprinter {
    pub fn get_fingerprint_of_file(
        &self,
        path: &Path,
        buffer: &mut Vec<u8>,
    ) -> Result<FileFingerprint, io::Error> {
        use FileFingerprint::*;

        match self.strategy {
            FingerprintStrategy::DevInode => {
                let file_handle = File::open(path)?;
                let dev = file_handle.portable_dev()?;
                let ino = file_handle.portable_ino()?;
                Ok(DevInode(dev, ino))
            }
            FingerprintStrategy::Checksum {
                ignored_header_bytes,
                bytes: _,
            }
            | FingerprintStrategy::FirstLineChecksum {
                ignored_header_bytes,
            } => {
                buffer.resize(self.max_line_length, 0u8);
                let mut fp = fs::File::open(path)?;
                fp.seek(SeekFrom::Start(ignored_header_bytes as u64))?;
                fingerprinter_read_until(fp, b'\n', buffer)?;
                let fingerprint = FINGERPRINT_CRC.checksum(&buffer[..]);
                Ok(FirstLineChecksum(fingerprint))
            }
        }
    }

    pub fn get_fingerprint_or_log_error(
        &self,
        path: &Path,
        buffer: &mut Vec<u8>,
        known_small_files: &mut HashSet<PathBuf>,
        emitter: &impl FileSourceInternalEvents,
    ) -> Option<FileFingerprint> {
        metadata(path)
            .and_then(|metadata| {
                if metadata.is_dir() {
                    Ok(None)
                } else {
                    self.get_fingerprint_of_file(path, buffer).map(Some)
                }
            })
            .map_err(|error| match error.kind() {
                io::ErrorKind::UnexpectedEof => {
                    if !known_small_files.contains(path) {
                        emitter.emit_file_checksum_failed(path);
                        known_small_files.insert(path.to_path_buf());
                    }
                }
                io::ErrorKind::NotFound => {
                    if !self.ignore_not_found {
                        emitter.emit_file_fingerprint_read_failed(path, error);
                    }
                }
                _ => {
                    emitter.emit_file_fingerprint_read_failed(path, error);
                }
            })
            .ok()
            .flatten()
    }

    pub fn get_bytes_checksum(
        &self,
        path: &Path,
        buffer: &mut Vec<u8>,
    ) -> Result<Option<FileFingerprint>, io::Error> {
        match self.strategy {
            FingerprintStrategy::Checksum {
                bytes,
                ignored_header_bytes,
            } => {
                buffer.resize(bytes, 0u8);
                let mut fp = fs::File::open(path)?;
                fp.seek(io::SeekFrom::Start(ignored_header_bytes as u64))?;
                fp.read_exact(&mut buffer[..bytes])?;
                let fingerprint = FINGERPRINT_CRC.checksum(&buffer[..]);
                Ok(Some(FileFingerprint::BytesChecksum(fingerprint)))
            }
            _ => Ok(None),
        }
    }
}

fn fingerprinter_read_until(mut r: impl Read, delim: u8, mut buf: &mut [u8]) -> io::Result<()> {
    'outer: while !buf.is_empty() {
        let mut read = match r.read(buf) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF reached")),
            Ok(n) => n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };

        match buf[..read].iter().position(|&c| c != b'\x00' ) {
            Some(pos) => {
                if pos > 0 {
                    buf.copy_within(pos..read, 0);
                    read -= pos;
                }
            },
            None => read = 0,
        }
        while read > 0 {
            // we search for the first occurence of the delimiter except if we do not
            // have read anything (files with leading NULLs and a newline without any
            // other byte)
            if let Some(pos) = buf[..read].iter().position(|&c| c == delim ) {
                if pos > 0 {
                    for el in &mut buf[(pos + 1)..] {
                        *el = 0;
                    }
                    break 'outer;
                } else {
                    // skip the newline because we have no byte read
                    buf.copy_within(pos+1..read, 0);
                    read -= pos+1;
                }
            }
        }

        buf = &mut buf[read..];
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::{FileSourceInternalEvents, FingerprintStrategy, Fingerprinter};
    use std::{collections::HashSet, fs, io::Error, path::Path, time::Duration};
    use tempfile::tempdir;

    #[test]
    fn test_checksum_fingerprint() {
        let fingerprinter = Fingerprinter {
            strategy: FingerprintStrategy::Checksum {
                bytes: 256,
                ignored_header_bytes: 0,
            },
            max_line_length: 1024,
            ignore_not_found: false,
        };

        let target_dir = tempdir().unwrap();
        let mut full_line_data = vec![b'x'; 256];
        full_line_data.push(b'\n');
        let not_full_line_data = vec![b'x'; 199];
        let empty_path = target_dir.path().join("empty.log");
        let full_line_path = target_dir.path().join("full_line.log");
        let duplicate_path = target_dir.path().join("duplicate.log");
        let not_full_line_path = target_dir.path().join("not_full_line.log");
        fs::write(&empty_path, &[]).unwrap();
        fs::write(&full_line_path, &full_line_data).unwrap();
        fs::write(&duplicate_path, &full_line_data).unwrap();
        fs::write(&not_full_line_path, &not_full_line_data).unwrap();

        let mut buf = Vec::new();
        assert!(fingerprinter
            .get_fingerprint_of_file(&empty_path, &mut buf)
            .is_err());
        assert!(fingerprinter
            .get_fingerprint_of_file(&full_line_path, &mut buf)
            .is_ok());
        assert!(fingerprinter
            .get_fingerprint_of_file(&not_full_line_path, &mut buf)
            .is_err());
        assert_eq!(
            fingerprinter
                .get_fingerprint_of_file(&full_line_path, &mut buf)
                .unwrap(),
            fingerprinter
                .get_fingerprint_of_file(&duplicate_path, &mut buf)
                .unwrap(),
        );
    }

    #[test]
    fn test_checksum_fingerprint_zerofilled() {
        let fingerprinter = Fingerprinter {
            strategy: FingerprintStrategy::FirstLineChecksum {
                ignored_header_bytes: 0,
            },
            max_line_length: 1024,
            ignore_not_found: false,
        };

        let target_dir = tempdir().unwrap();
        let mut many_zeros = vec![b'\x00'; 15000];
        let dat1 = vec![b'a', b'b', b'c'];
        many_zeros.push(b'x');
        many_zeros.append(&mut dat1.clone());
        many_zeros.push(b'\n');
        // many_zeros has 15000 leading zerors, followed by "xabc\n"
        let mut more_zeros = vec![b'\x00'; 16000];
        more_zeros.push(b'\n');
        more_zeros.push(b'x');
        more_zeros.append(&mut dat1.clone());
        more_zeros.push(b'\n');
        // more_zeros has a 16000 leading zeros, followed by "\nxabc\n"


        let mut more_zeros_diff = vec![b'\x00'; 16000];
        more_zeros_diff.push(b'y');
        more_zeros_diff.push(b'\n');

        let many_path = target_dir.path().join("many.log");
        let more_path = target_dir.path().join("more.log");
        let diff_path = target_dir.path().join("morediff.log");

        fs::write(&many_path, &many_zeros).unwrap();
        fs::write(&more_path, &more_zeros).unwrap();
        fs::write(&diff_path, &more_zeros_diff).unwrap();

        let mut buf = Vec::new();

        assert!(fingerprinter
            .get_fingerprint_of_file(&many_path, &mut buf).is_ok());
        assert!(fingerprinter
            .get_fingerprint_of_file(&more_path, &mut buf).is_ok());
        debug_assert_eq!(
            fingerprinter
            .get_fingerprint_of_file(&many_path, &mut buf).unwrap().clone(),
            fingerprinter
            .get_fingerprint_of_file(&more_path, &mut buf).unwrap().clone(),
        );
        assert_ne!(
            fingerprinter
            .get_fingerprint_of_file(&many_path, &mut buf).unwrap(),
            fingerprinter
            .get_fingerprint_of_file(&diff_path, &mut buf).unwrap(),
        );
        assert_ne!(
            fingerprinter
            .get_fingerprint_of_file(&more_path, &mut buf).unwrap(),
            fingerprinter
            .get_fingerprint_of_file(&diff_path, &mut buf).unwrap(),
        );
    }

    #[test]
    fn test_first_line_checksum_fingerprint() {
        let max_line_length = 64;
        let fingerprinter = Fingerprinter {
            strategy: FingerprintStrategy::FirstLineChecksum {
                ignored_header_bytes: 0,
            },
            max_line_length,
            ignore_not_found: false,
        };

        let target_dir = tempdir().unwrap();
        let prepare_test = |file: &str, contents: &[u8]| {
            let path = target_dir.path().join(file);
            fs::write(&path, contents).unwrap();
            path
        };
        let prepare_test_long = |file: &str, amount| {
            prepare_test(
                file,
                b"hello world "
                    .iter()
                    .cloned()
                    .cycle()
                    .clone()
                    .take(amount)
                    .collect::<Box<_>>()
                    .as_ref(),
            )
        };

        let empty = prepare_test("empty.log", b"");
        let incomlete_line = prepare_test("incomlete_line.log", b"missing newline char");
        let one_line = prepare_test("one_line.log", b"\x00\x00hello world\n");
        let one_line_duplicate = prepare_test("one_line_duplicate.log", b"\x00hello world\n");
        let one_line_continued =
            prepare_test("one_line_continued.log", b"hello world\nthe next line\n");
        let different_two_lines = prepare_test("different_two_lines.log", b"line one\nline two\n");

        let exactly_max_line_length =
            prepare_test_long("exactly_max_line_length.log", max_line_length);
        let exceeding_max_line_length =
            prepare_test_long("exceeding_max_line_length.log", max_line_length + 1);
        let incomplete_under_max_line_length_by_one = prepare_test_long(
            "incomplete_under_max_line_length_by_one.log",
            max_line_length - 1,
        );

        let mut buf = Vec::new();
        let mut run = move |path| fingerprinter.get_fingerprint_of_file(path, &mut buf);

        assert!(run(&empty).is_err());
        assert!(run(&incomlete_line).is_err());
        assert!(run(&incomplete_under_max_line_length_by_one).is_err());

        assert!(run(&one_line).is_ok());
        assert!(run(&one_line_duplicate).is_ok());
        assert!(run(&one_line_continued).is_ok());
        assert!(run(&different_two_lines).is_ok());
        assert!(run(&exactly_max_line_length).is_ok());
        assert!(run(&exceeding_max_line_length).is_ok());

        assert_eq!(run(&one_line).unwrap(), run(&one_line_duplicate).unwrap());
        assert_eq!(run(&one_line).unwrap(), run(&one_line_continued).unwrap());

        assert_ne!(run(&one_line).unwrap(), run(&different_two_lines).unwrap());

        assert_eq!(
            run(&exactly_max_line_length).unwrap(),
            run(&exceeding_max_line_length).unwrap()
        );
    }

    #[test]
    fn test_inode_fingerprint() {
        let fingerprinter = Fingerprinter {
            strategy: FingerprintStrategy::DevInode,
            max_line_length: 42,
            ignore_not_found: false,
        };

        let target_dir = tempdir().unwrap();
        let small_data = vec![b'x'; 1];
        let medium_data = vec![b'x'; 256];
        let empty_path = target_dir.path().join("empty.log");
        let small_path = target_dir.path().join("small.log");
        let medium_path = target_dir.path().join("medium.log");
        let duplicate_path = target_dir.path().join("duplicate.log");
        fs::write(&empty_path, &[]).unwrap();
        fs::write(&small_path, &small_data).unwrap();
        fs::write(&medium_path, &medium_data).unwrap();
        fs::write(&duplicate_path, &medium_data).unwrap();

        let mut buf = Vec::new();
        assert!(fingerprinter
            .get_fingerprint_of_file(&empty_path, &mut buf)
            .is_ok());
        assert!(fingerprinter
            .get_fingerprint_of_file(&small_path, &mut buf)
            .is_ok());
        assert_ne!(
            fingerprinter
                .get_fingerprint_of_file(&medium_path, &mut buf)
                .unwrap(),
            fingerprinter
                .get_fingerprint_of_file(&duplicate_path, &mut buf)
                .unwrap()
        );
    }

    #[test]
    fn no_error_on_dir() {
        let target_dir = tempdir().unwrap();
        let fingerprinter = Fingerprinter {
            strategy: FingerprintStrategy::Checksum {
                bytes: 256,
                ignored_header_bytes: 0,
            },
            max_line_length: 1024,
            ignore_not_found: false,
        };

        let mut buf = Vec::new();
        let mut small_files = HashSet::new();
        assert!(fingerprinter
            .get_fingerprint_or_log_error(
                &target_dir.path().to_owned(),
                &mut buf,
                &mut small_files,
                &NoErrors
            )
            .is_none());
    }

    #[derive(Clone)]
    struct NoErrors;

    impl FileSourceInternalEvents for NoErrors {
        fn emit_file_added(&self, _: &Path) {}

        fn emit_file_resumed(&self, _: &Path, _: u64) {}

        fn emit_file_watch_failed(&self, _: &Path, _: Error) {
            panic!();
        }

        fn emit_file_unwatched(&self, _: &Path) {}

        fn emit_file_deleted(&self, _: &Path) {}

        fn emit_file_delete_failed(&self, _: &Path, _: Error) {
            panic!();
        }

        fn emit_file_fingerprint_read_failed(&self, _: &Path, _: Error) {
            panic!();
        }

        fn emit_file_checkpointed(&self, _: usize, _: Duration) {}

        fn emit_file_checksum_failed(&self, _: &Path) {
            panic!();
        }

        fn emit_file_checkpoint_write_failed(&self, _: Error) {
            panic!();
        }

        fn emit_files_open(&self, _: usize) {}

        fn emit_path_globbing_failed(&self, _: &Path, _: &Error) {}
    }
}
