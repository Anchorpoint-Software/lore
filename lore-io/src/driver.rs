// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Driver construction and backend dispatch.
//!
//! The backend surface is the set of operations [`DriverInner`]'s match arms call: `open`,
//! `read_at`, `read_exact_at`, `write_at`, `write_all_at`, `read_file_bytes`, `write_file_bytes`,
//! `sync`, `metadata`, `file_metadata`, `set_len`, `rename`, `remove_file` and `create_dir_all`.
//! A backend implements all fourteen as inherent methods on its own type.
//!
//! Every operation dispatches, including the metadata ones a completion backend will keep on the
//! syscall pool anyway — a ring-submitted `statx` is punted to a kernel worker making the same
//! blocking call, so it buys no parallelism. Routing them anyway is what makes a backend able to
//! override any operation and a driver instance self-contained, rather than partly bypassed. It
//! also leaves [`crate::pool`] reachable only from the backends.
//!
//! There is no trait, and none is needed to make that complete: adding a variant to
//! [`DriverInner`] makes every match here non-exhaustive, and filling those arms in requires
//! each operation to exist with a compatible signature, so a backend cannot be half-wired. What
//! a trait would add over that is a single place to read the surface and a bar on signatures
//! drifting between backends — worth revisiting when there is a second one to compare against.
//!
//! Dispatch is static. The enum holds backend values rather than trait objects, so each arm
//! binds a concrete type and the call inlines as any inherent method would; no vtable exists on
//! this path.
//!
//! What the arms cannot check is behaviour, which is what `tests/conformance.rs` is for: adding a
//! backend to its `drivers()` list runs every case against it.
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;

use bytes::Bytes;

use crate::buffer::StableBuf;
use crate::file::IoFile;
use crate::file::OpenOptions;
use crate::psync::PsyncDriver;

/// Largest file the whole-file operations accept.
///
/// [`IoDriver::read_file_bytes`] and [`IoDriver::write_file_bytes`] exist to keep a scan over
/// many small files at one dispatch each. Both hold a pool thread for the whole transfer and hold
/// the whole file resident, so reaching for them with a large file would occupy one of at most
/// `min(2 × cores, 16)` threads for its duration. A caller with a large file wants [`open`] plus
/// [`read_exact_at`] or [`write_all_at`], which read and write a bounded length at a time.
///
/// [`open`]: IoDriver::open
/// [`read_exact_at`]: crate::IoFile::read_exact_at
/// [`write_all_at`]: crate::IoFile::write_all_at
pub const WHOLE_FILE_LIMIT: usize = 8 * 1024 * 1024;

pub(crate) fn check_whole_file_len(len: usize) -> std::io::Result<()> {
    if len > WHOLE_FILE_LIMIT {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{len} bytes exceeds the {WHOLE_FILE_LIMIT} byte whole-file limit; \
                 open the file and use read_exact_at or write_all_at"
            ),
        ));
    }
    Ok(())
}

/// Backend selection for an [`IoDriver`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    /// Probe for the best available backend.
    Auto,
    /// Positional syscalls on the bounded syscall pool.
    Psync,
}

pub(crate) enum DriverInner {
    Psync(PsyncDriver),
}

/// Parses a `LORE_IO_BACKEND` value. Separate from reading the variable so the accepted set and
/// the error are testable without a process-global environment.
fn backend_kind_from_value(value: &str) -> std::io::Result<BackendKind> {
    match value.to_ascii_lowercase().as_str() {
        "" | "auto" => Ok(BackendKind::Auto),
        "psync" => Ok(BackendKind::Psync),
        other => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unsupported LORE_IO_BACKEND \"{other}\" (supported: auto, psync)"),
        )),
    }
}

/// A file I/O driver instance dispatching to one backend.
///
/// Cloning is cheap and clones share the backend. Most code uses
/// [`IoDriver::global`]; tests and benchmarks construct instances per
/// backend.
#[derive(Clone)]
pub struct IoDriver {
    inner: Arc<DriverInner>,
}

impl std::fmt::Debug for IoDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IoDriver")
            .field("backend", &self.backend_name())
            .finish()
    }
}

impl IoDriver {
    pub fn new(kind: BackendKind) -> std::io::Result<IoDriver> {
        let inner = match kind {
            BackendKind::Auto | BackendKind::Psync => DriverInner::Psync(PsyncDriver),
        };
        Ok(IoDriver {
            inner: Arc::new(inner),
        })
    }

    /// Constructs a driver honoring the `LORE_IO_BACKEND` environment
    /// variable (`auto` | `psync`), failing on an unrecognised value.
    pub fn from_env() -> std::io::Result<IoDriver> {
        let kind = match std::env::var("LORE_IO_BACKEND") {
            Ok(value) => backend_kind_from_value(&value)?,
            Err(_) => BackendKind::Auto,
        };
        IoDriver::new(kind)
    }

    /// The process-wide driver, selected once from the environment.
    ///
    /// An unrecognised `LORE_IO_BACKEND` reports the problem and falls back to the probed
    /// backend rather than failing. The variable exists for diagnosis and rollback, and this
    /// crate runs inside host applications: a typo in it should not be able to take the host
    /// down on its first file read. Callers that would rather refuse to run misconfigured use
    /// [`from_env`](Self::from_env), which returns the error.
    pub fn global() -> &'static IoDriver {
        static GLOBAL: OnceLock<IoDriver> = OnceLock::new();
        GLOBAL.get_or_init(|| {
            IoDriver::from_env().unwrap_or_else(|error| {
                eprintln!("lore-io: {error}; using the probed backend instead");
                IoDriver::new(BackendKind::Auto).expect("the probed backend is always available")
            })
        })
    }

    /// The name of the selected backend, for logs and benchmarks.
    pub fn backend_name(&self) -> &'static str {
        match &*self.inner {
            DriverInner::Psync(_) => "psync",
        }
    }

    pub async fn open(
        &self,
        path: impl AsRef<Path>,
        options: &OpenOptions,
    ) -> std::io::Result<IoFile> {
        let std_options = options.to_std();
        let path = path.as_ref().to_path_buf();
        let file = match &*self.inner {
            DriverInner::Psync(driver) => driver.open(std_options, path).await?,
        };
        Ok(IoFile::new(self.clone(), Arc::new(file)))
    }

    pub async fn metadata(&self, path: impl AsRef<Path>) -> std::io::Result<std::fs::Metadata> {
        let path = path.as_ref().to_path_buf();
        match &*self.inner {
            DriverInner::Psync(driver) => driver.metadata(path).await,
        }
    }

    pub async fn rename(
        &self,
        from: impl AsRef<Path>,
        to: impl AsRef<Path>,
    ) -> std::io::Result<()> {
        let from = from.as_ref().to_path_buf();
        let to = to.as_ref().to_path_buf();
        match &*self.inner {
            DriverInner::Psync(driver) => driver.rename(from, to).await,
        }
    }

    pub async fn remove_file(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let path = path.as_ref().to_path_buf();
        match &*self.inner {
            DriverInner::Psync(driver) => driver.remove_file(path).await,
        }
    }

    pub async fn create_dir_all(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let path = path.as_ref().to_path_buf();
        match &*self.inner {
            DriverInner::Psync(driver) => driver.create_dir_all(path).await,
        }
    }

    pub(crate) async fn file_metadata_raw(
        &self,
        file: Arc<File>,
    ) -> std::io::Result<std::fs::Metadata> {
        match &*self.inner {
            DriverInner::Psync(driver) => driver.file_metadata(file).await,
        }
    }

    pub(crate) async fn set_len_raw(&self, file: Arc<File>, len: u64) -> std::io::Result<()> {
        match &*self.inner {
            DriverInner::Psync(driver) => driver.set_len(file, len).await,
        }
    }

    /// Reads an entire file into pooled memory as a single backend
    /// dispatch — open, stat, read to the stat length, close — and
    /// returns it as [`Bytes`] without copying.
    ///
    /// The whole-file read twin of
    /// [`write_file_bytes`](Self::write_file_bytes): small-file scans pay
    /// one dispatch per file instead of separate open and read round
    /// trips.
    ///
    /// Fails with `InvalidInput` for a file larger than [`WHOLE_FILE_LIMIT`].
    pub async fn read_file_bytes(&self, path: impl AsRef<Path>) -> std::io::Result<Bytes> {
        let path = path.as_ref().to_path_buf();
        match &*self.inner {
            DriverInner::Psync(driver) => driver.read_file_bytes(path).await,
        }
    }

    /// Writes `data` as the entire contents of `path` in a single backend
    /// dispatch — create (or truncate), write, optionally sync file data,
    /// stat, close — and returns the resulting metadata.
    ///
    /// The whole-file write twin of
    /// [`read_file_bytes`](Self::read_file_bytes), matching the atomic
    /// whole-file write pattern used by the storage layer.
    ///
    /// Fails with `InvalidInput` for data larger than [`WHOLE_FILE_LIMIT`]. The check runs
    /// before the file is opened, so a rejected call cannot have truncated an existing file.
    pub async fn write_file_bytes(
        &self,
        path: impl AsRef<Path>,
        data: Bytes,
        durable: bool,
    ) -> std::io::Result<std::fs::Metadata> {
        check_whole_file_len(data.len())?;
        let path = path.as_ref().to_path_buf();
        match &*self.inner {
            DriverInner::Psync(driver) => driver.write_file_bytes(path, data, durable).await,
        }
    }

    pub(crate) async fn read_at_raw(
        &self,
        file: Arc<File>,
        max_len: usize,
        offset: u64,
    ) -> std::io::Result<Bytes> {
        match &*self.inner {
            DriverInner::Psync(driver) => driver.read_at(file, max_len, offset).await,
        }
    }

    pub(crate) async fn read_exact_at_raw(
        &self,
        file: Arc<File>,
        len: usize,
        offset: u64,
    ) -> std::io::Result<Bytes> {
        match &*self.inner {
            DriverInner::Psync(driver) => driver.read_exact_at(file, len, offset).await,
        }
    }

    pub(crate) async fn write_all_at_raw<B: StableBuf>(
        &self,
        file: Arc<File>,
        buffer: B,
        len: usize,
        offset: u64,
    ) -> std::io::Result<B> {
        match &*self.inner {
            DriverInner::Psync(driver) => driver.write_all_at(file, buffer, len, offset).await,
        }
    }

    pub(crate) async fn write_at_raw<B: StableBuf>(
        &self,
        file: Arc<File>,
        buffer: B,
        buffer_offset: usize,
        len: usize,
        offset: u64,
    ) -> std::io::Result<(B, usize)> {
        match &*self.inner {
            DriverInner::Psync(driver) => {
                driver
                    .write_at(file, buffer, buffer_offset, len, offset)
                    .await
            }
        }
    }

    pub(crate) async fn sync_raw(&self, file: Arc<File>, data_only: bool) -> std::io::Result<()> {
        match &*self.inner {
            DriverInner::Psync(driver) => driver.sync(file, data_only).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recognised_backend_value_selects_it() {
        assert_eq!(
            backend_kind_from_value("psync").unwrap(),
            BackendKind::Psync
        );
        assert_eq!(
            backend_kind_from_value("PSYNC").unwrap(),
            BackendKind::Psync
        );
        assert_eq!(backend_kind_from_value("auto").unwrap(), BackendKind::Auto);
        assert_eq!(backend_kind_from_value("").unwrap(), BackendKind::Auto);
    }

    /// The value names the accepted set, because the caller reaching for this variable is
    /// diagnosing something and a bare rejection tells them nothing.
    #[test]
    fn an_unrecognised_backend_value_is_a_reportable_error() {
        let error = backend_kind_from_value("uring")
            .expect_err("a backend that does not exist yet must not be accepted");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        let message = format!("{error}");
        assert!(message.contains("uring"), "{message}");
        assert!(message.contains("supported: auto, psync"), "{message}");
    }

    /// Whatever the environment holds, the process-wide driver resolves rather than panicking.
    #[test]
    fn the_global_driver_always_resolves() {
        assert_eq!(IoDriver::global().backend_name(), "psync");
    }
}
