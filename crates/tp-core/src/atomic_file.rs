//! Atomic file replacement that survives a Windows sharing race.
//!
//! Replacing a file goes through `NamedTempFile::persist`, which is
//! `MoveFileEx(MOVEFILE_REPLACE_EXISTING)` on Windows. That call fails while
//! any other handle still holds the destination, and a virus scanner or an
//! exiting sibling process holds one briefly, so a correctly locked writer
//! still loses the replace. Retrying is the remedy; nothing here relaxes the
//! atomicity or the owner-only permissions the callers already established.

use std::io;
use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

use tempfile::NamedTempFile;

/// Bounded backoff, about 1.1 seconds in total. A destination held longer than
/// that is not a scanner window and must surface as a real failure.
const RETRY_DELAYS: [Duration; 8] = [
    Duration::from_millis(10),
    Duration::from_millis(20),
    Duration::from_millis(40),
    Duration::from_millis(80),
    Duration::from_millis(160),
    Duration::from_millis(200),
    Duration::from_millis(300),
    Duration::from_millis(300),
];

/// Replaces `path` with `temporary`, retrying only the transient Windows
/// sharing errors. Every other error fails on the first attempt.
pub fn persist_atomically(temporary: NamedTempFile, path: &Path) -> io::Result<()> {
    let mut pending = temporary;
    for delay in RETRY_DELAYS {
        match pending.persist(path) {
            Ok(_) => return Ok(()),
            Err(error) if is_transient_replace_error(&error.error) => {
                pending = error.file;
                sleep(delay);
            }
            Err(error) => return Err(error.error),
        }
    }
    pending
        .persist(path)
        .map(|_| ())
        .map_err(|error| error.error)
}

/// True only for the Windows error codes raised while another handle still
/// holds the destination.
fn is_transient_replace_error(error: &io::Error) -> bool {
    #[cfg(windows)]
    {
        const ERROR_ACCESS_DENIED: i32 = 5;
        const ERROR_SHARING_VIOLATION: i32 = 32;
        const ERROR_LOCK_VIOLATION: i32 = 33;
        matches!(
            error.raw_os_error(),
            Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
        )
    }
    #[cfg(not(windows))]
    {
        let _ = error;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write as _;

    #[test]
    fn replacing_an_existing_file_publishes_the_new_bytes() {
        let directory = tempfile::tempdir().expect("temp dir");
        let destination = directory.path().join("owner.yaml");
        fs::write(&destination, b"old").expect("seed destination");

        let mut temporary = NamedTempFile::new_in(directory.path()).expect("temp file");
        temporary.write_all(b"new").expect("write temp");

        persist_atomically(temporary, &destination).expect("replace destination");

        assert_eq!(fs::read(&destination).expect("read destination"), b"new");
    }

    #[test]
    fn creating_a_missing_file_publishes_the_new_bytes() {
        let directory = tempfile::tempdir().expect("temp dir");
        let destination = directory.path().join("owner.yaml");

        let mut temporary = NamedTempFile::new_in(directory.path()).expect("temp file");
        temporary.write_all(b"new").expect("write temp");

        persist_atomically(temporary, &destination).expect("create destination");

        assert_eq!(fs::read(&destination).expect("read destination"), b"new");
    }

    #[test]
    fn only_the_windows_sharing_codes_are_retried() {
        let sharing_codes = [5, 32, 33];
        let unrelated_codes = [2, 13, 87];

        for code in sharing_codes {
            let retried = is_transient_replace_error(&io::Error::from_raw_os_error(code));
            assert_eq!(
                retried,
                cfg!(windows),
                "raw OS error {code} must be retried on Windows only"
            );
        }
        for code in unrelated_codes {
            assert!(
                !is_transient_replace_error(&io::Error::from_raw_os_error(code)),
                "raw OS error {code} is not a sharing race and must fail immediately"
            );
        }
    }

    #[test]
    fn the_backoff_stays_bounded() {
        let total: Duration = RETRY_DELAYS.iter().sum();
        assert!(
            total <= Duration::from_millis(1200),
            "a held destination must surface as a failure, not a long stall"
        );
    }
}
