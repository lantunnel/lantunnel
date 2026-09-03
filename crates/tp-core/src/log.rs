//! File rotation writer built from [`LogConfig`].
//!
//! `tracing-appender` only supports time-based rotation (daily/hourly),
//! so the `max_size`, `max_age`, `max_backups`, and `compress` knobs in
//! [`crate::config::LogConfig`] were documented but not enforced. Here we
//! wrap [`file_rotate::FileRotate`] so every knob actually takes effect:
//!
//! * `max_size` (MB) → [`ContentLimit::BytesSurpassed`] — log is rotated
//!   after the line that pushed it past the cap, so a rotation boundary
//!   never splits a single log line.
//! * `max_age` (days) + `max_backups` (count) → `max_backups` is enforced
//!   by `file-rotate` at rotation time, and [`prune_rolling_logs`] also
//!   applies both count and age caps to already-rotated files.
//! * `compress` (bool) → [`Compression::OnRotate(1)`] keeps the just-
//!   rotated file uncompressed (still the most likely one you'll `tail -f`)
//!   and gzips everything older.
//!
//! The writer uses [`AppendTimestamp`] suffixes so file names stay
//! monotonically sortable regardless of rotation cause.
//!
//! Callers should wrap the returned writer in
//! [`tracing_appender::non_blocking`] so the tracing hot path doesn't
//! block on rename/gzip I/O during rotation.

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration as StdDuration, Instant},
};

use file_rotate::{
    compression::Compression,
    suffix::{AppendTimestamp, FileLimit, SuffixScheme},
    ContentLimit, FileRotate,
};

use crate::config::LogConfig;

const RETENTION_CHECK_INTERVAL: StdDuration = StdDuration::from_secs(60);

/// Effective `(FileLimit, Compression, content_limit_bytes)` derived from
/// [`LogConfig`]. Extracted from [`build_rolling_writer`] so we can unit-test
/// the mapping without touching the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationParams {
    /// The cap enforced directly by `file-rotate`.
    ///
    /// When both `max_backups` and `max_age` are set, this is
    /// [`FileLimitKind::MaxFiles`] so noisy clients cannot accumulate an
    /// unbounded number of same-day rotations. [`prune_rolling_logs`] enforces
    /// the age cap separately.
    pub file_limit_kind: FileLimitKind,
    /// `true` when compression of rotated files should be enabled.
    pub compress: bool,
    /// Rotate after a single write pushes the current file past this many
    /// bytes. `0` means "no size-based rotation — rely solely on age /
    /// external truncation" (then [`ContentLimit::None`] is used).
    pub max_bytes: usize,
}

/// Simpler, testable view of `FileLimit` — `FileLimit` itself is an enum
/// whose variants carry non-`PartialEq` payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileLimitKind {
    /// Keep at most `n` rotated files.
    MaxFiles(usize),
    /// Delete rotated files older than `days` days.
    AgeDays(i64),
    /// No cap — file-rotate will still create suffixed files but never
    /// garbage-collect them.
    Unlimited,
}

/// Resolve the [`LogConfig`] knobs to file-rotate parameters.
///
/// Precedence rules (documented here so callers don't have to read the crate
/// source):
///
/// 1. `max_age > 0` and `max_backups > 0` — **backups win** for the
///    `file-rotate` limit so high-volume logs cannot keep unlimited same-day
///    files. [`prune_rolling_logs`] still applies the age cap separately.
/// 2. Only `max_age > 0` — [`FileLimitKind::AgeDays`].
/// 3. Only `max_backups > 0` — [`FileLimitKind::MaxFiles`].
/// 4. Neither — [`FileLimitKind::Unlimited`].
pub fn rotation_params(cfg: &LogConfig) -> RotationParams {
    let file_limit_kind = pick_file_limit(cfg.max_age, cfg.max_backups);
    let max_bytes = (cfg.max_size as usize).saturating_mul(1024 * 1024);
    RotationParams {
        file_limit_kind,
        compress: cfg.compress,
        max_bytes,
    }
}

/// Pick the cap that `file-rotate` can enforce immediately at rotation time.
pub fn pick_file_limit(max_age: u32, max_backups: u32) -> FileLimitKind {
    if max_backups > 0 {
        FileLimitKind::MaxFiles(max_backups as usize)
    } else if max_age > 0 {
        FileLimitKind::AgeDays(max_age as i64)
    } else {
        FileLimitKind::Unlimited
    }
}

/// Build a rolling file writer honoring every knob in `cfg`.
///
/// `log_path` is the *base* path (e.g. `/var/log/gateway.log`); rotated
/// files land next to it with timestamp suffixes.
///
/// Returns a [`RollingLogWriter`] that implements [`std::io::Write`] and can
/// be moved straight into [`tracing_appender::non_blocking`].
pub fn build_rolling_writer(
    log_path: impl AsRef<Path>,
    cfg: &LogConfig,
) -> io::Result<RollingLogWriter> {
    RollingLogWriter::new(log_path.as_ref(), cfg)
}

/// Remove rotated log files that exceed the configured count or age caps.
///
/// `file-rotate` only enforces its own [`FileLimit`] when a rotation happens.
/// This helper runs during writer startup and periodically while the writer is
/// active, so stale rotations do not sit around indefinitely when the current
/// file stays under `max_size` for days.
pub fn prune_rolling_logs(log_path: impl AsRef<Path>, cfg: &LogConfig) -> io::Result<()> {
    if cfg.max_backups == 0 && cfg.max_age == 0 {
        return Ok(());
    }

    let log_path = log_path.as_ref();
    let suffixes = AppendTimestamp::default(FileLimit::Unlimited).scan_suffixes(log_path);
    let cutoff = if cfg.max_age > 0 {
        Some(
            (chrono::Local::now() - chrono::Duration::days(cfg.max_age as i64))
                .format("%Y%m%dT%H%M%S")
                .to_string(),
        )
    } else {
        None
    };

    let mut first_error: Option<io::Error> = None;
    for (idx, info) in suffixes.iter().enumerate() {
        let over_count = cfg.max_backups > 0 && idx >= cfg.max_backups as usize;
        let over_age = cutoff
            .as_ref()
            .is_some_and(|cutoff| info.suffix.timestamp < *cutoff);
        if !over_count && !over_age {
            continue;
        }

        let path = info.to_path(log_path);
        if let Err(err) = std::fs::remove_file(&path) {
            if first_error.is_none() && err.kind() != io::ErrorKind::NotFound {
                first_error = Some(err);
            }
        }
    }

    match first_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Rolling writer that wraps `file-rotate` and periodically resynchronizes
/// retention state with the filesystem.
pub struct RollingLogWriter {
    log_path: PathBuf,
    cfg: LogConfig,
    inner: FileRotate<AppendTimestamp>,
    next_retention_check: Instant,
}

impl RollingLogWriter {
    fn new(log_path: &Path, cfg: &LogConfig) -> io::Result<Self> {
        if let Some(parent) = log_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        prune_rolling_logs(log_path, cfg)?;
        Ok(Self {
            log_path: log_path.to_path_buf(),
            cfg: cfg.clone(),
            inner: make_file_rotate(log_path, cfg)?,
            next_retention_check: Instant::now() + RETENTION_CHECK_INTERVAL,
        })
    }

    fn maybe_prune(&mut self) -> io::Result<()> {
        if Instant::now() < self.next_retention_check {
            return Ok(());
        }
        self.next_retention_check = Instant::now() + RETENTION_CHECK_INTERVAL;
        self.inner.flush()?;

        // Best-effort cleanup: a transient delete failure should not break
        // logging. Rebuild the rotator either way so its scanned suffix set
        // matches the files still present on disk.
        let _ = prune_rolling_logs(&self.log_path, &self.cfg);
        self.inner = make_file_rotate(&self.log_path, &self.cfg)?;
        Ok(())
    }
}

impl io::Write for RollingLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.maybe_prune()?;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn make_file_rotate(log_path: &Path, cfg: &LogConfig) -> io::Result<FileRotate<AppendTimestamp>> {
    let params = rotation_params(cfg);

    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let file_limit = match params.file_limit_kind {
        FileLimitKind::MaxFiles(n) => FileLimit::MaxFiles(n),
        FileLimitKind::AgeDays(d) => FileLimit::Age(chrono::Duration::days(d)),
        // file-rotate has no "keep everything" mode; pick an absurdly
        // large MaxFiles so we effectively never prune. This matches the
        // historical gateway behavior which also didn't enforce a cap.
        FileLimitKind::Unlimited => FileLimit::MaxFiles(usize::MAX),
    };

    let content_limit = if params.max_bytes > 0 {
        ContentLimit::BytesSurpassed(params.max_bytes)
    } else {
        ContentLimit::None
    };

    let compression = if params.compress {
        // Keep 1 rotated file uncompressed — it's the freshest and by far
        // the most useful to `tail -f` when diagnosing an incident.
        Compression::OnRotate(1)
    } else {
        Compression::None
    };

    Ok(FileRotate::new(
        log_path,
        AppendTimestamp::default(file_limit),
        content_limit,
        compression,
        None,
    ))
}

/// Convenience helper: join `dir.join(format!("{prefix}.{suffix}"))`,
/// matching the historical file naming used by the gateway and client
/// binaries (`gateway.log`, `client.log`).
pub fn default_log_path(dir: impl AsRef<Path>, prefix: &str, suffix: &str) -> PathBuf {
    dir.as_ref().join(format!("{prefix}.{suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(max_size: u32, max_backups: u32, max_age: u32, compress: bool) -> LogConfig {
        LogConfig {
            level: "info".into(),
            format: "text".into(),
            output: "file".into(),
            file: Some("/tmp/test.log".into()),
            max_size,
            max_backups,
            max_age,
            compress,
        }
    }

    #[test]
    fn backups_beat_age_when_both_set() {
        let p = rotation_params(&cfg(100, 3, 7, true));
        assert_eq!(p.file_limit_kind, FileLimitKind::MaxFiles(3));
        assert!(p.compress);
        assert_eq!(p.max_bytes, 100 * 1024 * 1024);
    }

    #[test]
    fn only_backups() {
        let p = rotation_params(&cfg(50, 5, 0, false));
        assert_eq!(p.file_limit_kind, FileLimitKind::MaxFiles(5));
        assert!(!p.compress);
        assert_eq!(p.max_bytes, 50 * 1024 * 1024);
    }

    #[test]
    fn only_age() {
        let p = rotation_params(&cfg(0, 0, 14, false));
        assert_eq!(p.file_limit_kind, FileLimitKind::AgeDays(14));
        assert_eq!(p.max_bytes, 0);
    }

    #[test]
    fn neither_cap() {
        let p = rotation_params(&cfg(0, 0, 0, false));
        assert_eq!(p.file_limit_kind, FileLimitKind::Unlimited);
        assert_eq!(p.max_bytes, 0);
    }

    #[test]
    fn default_log_config_rotation_params() {
        // The production default (100 MB size / 3 backups / 7 days /
        // compress=true) should enforce the count cap in file-rotate. Age is
        // applied by prune_rolling_logs.
        let p = rotation_params(&LogConfig::default());
        assert_eq!(p.file_limit_kind, FileLimitKind::MaxFiles(3));
        assert_eq!(p.max_bytes, 100 * 1024 * 1024);
        assert!(p.compress);
    }

    #[test]
    fn max_size_overflow_is_saturating() {
        // u32::MAX MB would overflow usize on a 32-bit target. Guard
        // against silent panic.
        let mut c = cfg(u32::MAX, 0, 0, false);
        c.max_size = u32::MAX;
        let p = rotation_params(&c);
        // On 64-bit: ~4095 TiB — insanely large but well-defined.
        // On 32-bit: clamped to usize::MAX via saturating_mul.
        assert!(p.max_bytes > 0);
    }

    #[test]
    fn build_rolling_writer_smoke() {
        // Create a writer in tempdir, write a line, drop it, check file.
        let tmp = tempdir();
        let path = tmp.path.join("app.log");
        let cfg = cfg(0, 0, 0, false);
        let mut w = build_rolling_writer(&path, &cfg).unwrap();
        use std::io::Write;
        writeln!(w, "hello").unwrap();
        drop(w);
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("hello"));
    }

    #[test]
    fn prune_rolling_logs_applies_count_and_age_caps() {
        let tmp = tempdir();
        let path = tmp.path.join("app.log");
        std::fs::write(&path, "current").unwrap();

        let now = chrono::Local::now();
        let timestamps = [
            now - chrono::Duration::minutes(1),
            now - chrono::Duration::minutes(2),
            now - chrono::Duration::minutes(3),
            now - chrono::Duration::minutes(4),
            now - chrono::Duration::days(8),
        ];
        for ts in timestamps {
            std::fs::write(
                tmp.path
                    .join(format!("app.log.{}", ts.format("%Y%m%dT%H%M%S"))),
                "rotated",
            )
            .unwrap();
        }

        prune_rolling_logs(&path, &cfg(100, 3, 7, true)).unwrap();

        let mut remaining: Vec<String> = std::fs::read_dir(&tmp.path)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.starts_with("app.log."))
            .collect();
        remaining.sort();
        assert_eq!(remaining.len(), 3);
        assert!(remaining.iter().all(|name| !name.contains(
            &(now - chrono::Duration::days(8))
                .format("%Y%m%dT%H%M%S")
                .to_string()
        )));
    }

    #[test]
    fn build_rolling_writer_prunes_existing_rotations_on_startup() {
        let tmp = tempdir();
        let path = tmp.path.join("app.log");
        std::fs::write(&path, "current").unwrap();
        let now = chrono::Local::now();
        for offset in 1..=5 {
            std::fs::write(
                tmp.path.join(format!(
                    "app.log.{}",
                    (now - chrono::Duration::minutes(offset)).format("%Y%m%dT%H%M%S")
                )),
                "rotated",
            )
            .unwrap();
        }

        let _writer = build_rolling_writer(&path, &cfg(100, 3, 7, true)).unwrap();

        let remaining = std::fs::read_dir(&tmp.path)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("app.log."))
            .count();
        assert_eq!(remaining, 3);
    }

    // Ultra-light tempdir (avoids adding a dev-dep for a single test).
    struct TempDir {
        path: PathBuf,
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
    fn tempdir() -> TempDir {
        let p = std::env::temp_dir().join(format!(
            "tp-core-log-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        TempDir { path: p }
    }
}
