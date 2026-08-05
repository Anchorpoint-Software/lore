// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use bytes::BytesMut;
use lore_error_set::prelude::*;
use lore_io::IoDriver;
use lore_io::IoFile;
use lore_io::OpenOptions;
use tokio::sync::RwLock;
use tokio::sync::RwLockWriteGuard;
use tokio::task::JoinSet;

#[error_set]
pub enum PackfileError {}

#[derive(Debug)]
pub struct PackStoreRef {
    pub id: u32,
    pub offset: u32,
}

struct PackFile {
    id: u32,
    size: usize,
    file: Option<IoFile>,
    buffer: Vec<u8>,
    dirty: AtomicBool,
}

/// First wait between attempts at a transiently failed packfile operation, in milliseconds.
const RETRY_START_MILLIS: u64 = 10;

/// Longest wait the backoff grows to, in milliseconds.
const RETRY_MAXIMUM_MILLIS: u64 = 10_000;

/// Attempts before a transient failure is reported. With the schedule above this spends roughly
/// fifteen minutes on an operation that keeps failing, which is a deliberate budget: the process
/// holding the file is expected to let go, and failing the read would fail the caller's whole
/// operation.
const RETRY_LIMIT: usize = 100;

struct Retry {
    current: u64,
    maximum: u64,
    counter: usize,
    limit: usize,
}

impl Retry {
    fn new(start: u64, maximum: u64, limit: usize) -> Self {
        Retry {
            current: start,
            maximum,
            counter: 0,
            limit,
        }
    }

    async fn wait(&mut self) -> bool {
        tokio::time::sleep(std::time::Duration::from_millis(self.current)).await;
        self.current = std::cmp::min(self.current * 2, self.maximum);
        self.counter += 1;
        self.counter < self.limit
    }
}

/// Whether the operation is worth reissuing rather than reporting.
///
/// Positional reads and writes are idempotent, so reissuing is always safe. What the set buys is
/// the other direction: a permanent failure — a short read from a truncated packfile, an offset
/// past the end, a permission problem — is reported at once instead of after the whole retry
/// budget above.
fn is_transient(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
    ) || is_transient_for_platform(error)
}

/// Another process holding the file, or a byte range inside it, fails an operation that succeeds
/// as soon as it lets go — a virus scanner or a search indexer walking the store. A sharing
/// violation is raised when a handle is opened and a lock violation against a handle already open,
/// and both belong here because the packstore does both.
#[cfg(target_family = "windows")]
fn is_transient_for_platform(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code)
            if code == windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32
                || code == windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION as i32
    )
}

/// No platform-specific transient conditions: no kernel here fails a positional operation because
/// another process holds the file open.
#[cfg(not(target_family = "windows"))]
fn is_transient_for_platform(_error: &std::io::Error) -> bool {
    false
}

/// Runs `operation`, reissuing it with backoff for as long as it fails transiently.
///
/// Takes a plain closure returning a future rather than an `AsyncFnMut`: the latter's
/// higher-ranked lifetime defeats `Send` inference for the callers that spawn these operations.
async fn retry_transient<T, F, O>(mut operation: F) -> std::io::Result<T>
where
    F: FnMut() -> O,
    O: Future<Output = std::io::Result<T>>,
{
    let mut retry = Retry::new(RETRY_START_MILLIS, RETRY_MAXIMUM_MILLIS, RETRY_LIMIT);
    loop {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                if !is_transient(&error) || !retry.wait().await {
                    return Err(error);
                }
            }
        }
    }
}

async fn packfile_read(file: &IoFile, offset: usize, size: usize) -> Result<Bytes, PackfileError> {
    Ok(retry_transient(|| file.read_exact_at(size, offset as u64))
        .await
        .internal("Failed reading from packstore file")?)
}

async fn packfile_write(file: &IoFile, buffer: Bytes, offset: usize) -> Result<(), PackfileError> {
    retry_transient(|| file.write_all_at(buffer.clone(), offset as u64))
        .await
        .internal("Failed writing to packstore file")?;
    Ok(())
}

/// Maximum size of a single packfile
const PACKSTORE_SIZE_LIMIT: u64 = 3 * 1024 * 1024 * 1024;

pub struct PackStore {
    path: Option<PathBuf>,
    min_count: usize,
    packfile: RwLock<Vec<RwLock<PackFile>>>,
    writeable: RwLock<Vec<u32>>,
    /// Shared per-store GC counters; `resume()` feeds loaded packfile sizes into them
    /// so an over-cap store fires compaction without a startup scan. `None` for stores
    /// that don't participate in automatic GC (e.g. migration/scratch packstores).
    gc_counters: Option<Arc<crate::maintenance::GcCounters>>,
}

impl PackStore {
    pub fn new(
        path: Option<PathBuf>,
        min_count: usize,
        gc_counters: Option<Arc<crate::maintenance::GcCounters>>,
    ) -> Self {
        PackStore {
            path: path.map(|path| {
                let mut path = path;
                path.push("pack");
                path
            }),
            min_count,
            packfile: RwLock::default(),
            writeable: RwLock::default(),
            gc_counters,
        }
    }

    pub async fn resume(&self) -> Result<(), PackfileError> {
        let mut packfile = self.packfile.write().await;
        let mut writeable = self.writeable.write().await;

        if !writeable.is_empty() {
            return Ok(());
        }

        let mut loaded_size: u64 = 0;

        if let Some(path) = self.path.as_ref() {
            let path = path.clone();
            if !path.exists() {
                IoDriver::global()
                    .create_dir_all(&path)
                    .await
                    .internal(&format!(
                        "Failed to create packstore directory {}",
                        path.display()
                    ))?;
            }

            let paths = std::fs::read_dir(&path).internal(&format!(
                "Failed to read packstore directory {}",
                path.display()
            ))?;
            let mut packfile_count = 0;
            for entry in paths {
                let Ok(entry) = entry else {
                    continue;
                };
                let Ok(file_meta) = IoDriver::global().metadata(entry.path()).await else {
                    continue;
                };
                if !file_meta.is_file() {
                    continue;
                }

                let file_name = entry.file_name();
                let Some(file_id) = file_name.to_str() else {
                    continue;
                };
                let Ok(file_id) = file_id.parse::<u32>() else {
                    continue;
                };
                if file_id == 0 {
                    continue;
                }

                if file_id > packfile_count {
                    packfile_count = file_id;
                }
            }

            for index in 0..packfile_count {
                let file_id = index + 1;
                let file_path = path.join(file_id.to_string());
                let file_options = OpenOptions::new().read(true).write(true);
                // Prevent any other process from writing the file
                #[cfg(target_family = "windows")]
                let file_options = file_options
                    .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ);
                let file_options = if IoDriver::global()
                    .metadata(file_path.as_path())
                    .await
                    .is_ok()
                {
                    lore_base::lore_trace!("Resuming packfile {file_id}");
                    file_options
                } else {
                    lore_base::lore_trace!("Create packfile {file_id}");
                    file_options.create(true).truncate(true)
                };
                let file = IoDriver::global()
                    .open(file_path.as_path(), &file_options)
                    .await
                    .internal(&format!(
                        "Failed opening packstore file {}",
                        file_path.display()
                    ))?;
                let file_size = file
                    .metadata()
                    .await
                    .internal(&format!(
                        "Failed reading packstore file size {}",
                        file_path.display()
                    ))?
                    .len();

                let file = PackFile {
                    id: file_id,
                    size: file_size as usize,
                    file: Some(file),
                    buffer: vec![],
                    dirty: AtomicBool::new(false),
                };

                packfile.push(RwLock::new(file));
                loaded_size += file_size;

                if file_size < PACKSTORE_SIZE_LIMIT {
                    lore_base::lore_trace!("Packfile {file_id} is writeable");
                    writeable.push(file_id);
                }
            }
        }

        lore_base::lore_trace!("{} writable packfiles", writeable.len());

        let _ = self.fill_writeable(packfile, writeable).await;

        if let Some(gc) = &self.gc_counters {
            gc.add_loaded_size(loaded_size);
        }

        Ok(())
    }

    async fn mark_full(&self, id: u32) {
        let packfile = self.packfile.write().await;
        let mut writeable = self.writeable.write().await;
        for (index, writeable_id) in writeable.iter().enumerate() {
            if *writeable_id == id {
                writeable.swap_remove(index);
                break;
            }
        }

        let _ = self.fill_writeable(packfile, writeable).await;
    }

    async fn fill_writeable<'a>(
        &'a self,
        mut packfile: RwLockWriteGuard<'a, Vec<RwLock<PackFile>>>,
        mut writeable: RwLockWriteGuard<'a, Vec<u32>>,
    ) -> Result<(), PackfileError> {
        while writeable.len() < self.min_count {
            let index = packfile.len();
            let id = (index + 1) as u32;
            lore_base::lore_trace!("Create additional packfile {id}");

            let file = if let Some(path) = self.path.as_ref() {
                let file_path = path.join(id.to_string());
                let file_options = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true);
                // Prevent any other process from writing the file
                #[cfg(target_family = "windows")]
                let file_options = file_options
                    .share_mode(windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ);
                let file = IoDriver::global()
                    .open(file_path.as_path(), &file_options)
                    .await
                    .internal(&format!(
                        "Failed opening packstore file {}",
                        file_path.display()
                    ))?;

                PackFile {
                    id,
                    size: 0,
                    file: Some(file),
                    buffer: vec![],
                    dirty: AtomicBool::new(false),
                }
            } else {
                PackFile {
                    id,
                    size: 0,
                    file: None,
                    buffer: vec![],
                    dirty: AtomicBool::new(false),
                }
            };

            packfile.push(RwLock::new(file));
            writeable.push(id);
        }

        Ok(())
    }

    pub async fn stop_write(&self, id: u32) -> Result<usize, PackfileError> {
        if self.path.is_none() {
            return Err(PackfileError::internal("No more packfiles"));
        }

        if id == 0 {
            return Err(PackfileError::internal("Invalid packfile"));
        }

        let index = (id as usize) - 1;
        let current_size = {
            let mut packfiles = self.packfile.read().await;
            if packfiles.is_empty() {
                drop(packfiles);
                self.resume().await?;
                packfiles = self.packfile.read().await;
            }
            if index >= packfiles.len() {
                return Err(PackfileError::internal("No more packfiles"));
            }
            packfiles[index].read().await.size
        };

        self.mark_full(id).await;

        Ok(current_size)
    }

    pub async fn truncate(&self, id: u32) -> Result<(), PackfileError> {
        if self.path.is_none() {
            return Err(PackfileError::internal("No more packfiles"));
        }

        if id == 0 {
            return Err(PackfileError::internal("Invalid packfile"));
        }

        // Ensure packfile is not marked as writeable
        {
            let writeable = self.writeable.read().await;
            if writeable.contains(&id) {
                lore_base::lore_warn!("Tried truncating a writeable packfile {id}");
                return Err(PackfileError::internal(
                    "Cannot truncate a writeable packfile",
                ));
            }
        }

        {
            let mut packfile = self.packfile.write().await;

            let index = (id - 1) as usize;
            let packfile_count = packfile.len();
            if index >= packfile_count {
                return Err(PackfileError::internal("Invalid packfile"));
            }

            // Retire this packfile, caller guarantees nothing refers to it anymore so truncate it to zero
            if id as usize == packfile_count && packfile_count > self.min_count {
                // We can discard this packfile, it is the last packfile and we have enough remaining
                // packfiles without it to satisfy the min count requested
                drop(packfile.pop());

                if let Some(path) = self.path.as_ref() {
                    let file_path = path.join(id.to_string());
                    lore_base::lore_debug!(
                        "Discard compacted and truncated packfile {}",
                        file_path.display()
                    );
                    let _ = IoDriver::global().remove_file(file_path.as_path()).await;
                }

                return Ok(());
            }

            let mut packfile = packfile[index].write().await;

            packfile.dirty.store(false, Ordering::Release);
            if let Some(file) = packfile.file.as_ref() {
                let _ = file.set_len(0).await;
                let _ = file.sync_all().await;
            }
            packfile.buffer.clear();
            packfile.size = 0;
        }

        lore_base::lore_debug!("Truncated compacted packfile {id} to zero");

        // Mark the truncated packfile as writeable again
        {
            let mut writeable = self.writeable.write().await;
            if !writeable.contains(&id) {
                writeable.push(id);
            }
        }

        Ok(())
    }

    pub async fn load(&self, id: u32, offset: u32, size: u32) -> Result<Bytes, PackfileError> {
        if size == 0 {
            return Ok(Bytes::default());
        }
        if id == 0 {
            return Err(PackfileError::internal("Invalid packfile"));
        }

        let index = (id - 1) as usize;
        let size = size as usize;
        let offset = offset as usize;

        let mut packfiles = self.packfile.read().await;
        if packfiles.is_empty() {
            drop(packfiles);
            self.resume().await?;
            packfiles = self.packfile.read().await;
        }

        if index >= packfiles.len() {
            return Err(PackfileError::internal("Invalid packfile"));
        }

        let packfile = packfiles[index].read().await;
        if packfile.id != id {
            return Err(PackfileError::internal("Packfile ID mismatch index"));
        }

        // We got lock on the right packfile, load data
        if let Some(file) = packfile.file.as_ref() {
            return packfile_read(file, offset, size).await;
        }

        if (offset + size) > packfile.buffer.len() {
            return Err(PackfileError::internal(
                "Failed reading from packstore buffer, boundary violation",
            ));
        }

        Ok(Bytes::copy_from_slice(
            &packfile.buffer[offset..(offset + size)],
        ))
    }

    pub async fn store(&self, buffer: Bytes) -> Result<PackStoreRef, PackfileError> {
        let size = buffer.len();
        if size == 0 {
            return Err(PackfileError::internal(
                "Failed to store invalid zero sized buffer",
            ));
        }

        // Note that it is fine if multiple store operations end up on the same writeable packfiles
        // and both trigger the full-and-remove from writeable in parallel
        let mut packfiles = self.packfile.read().await;
        if packfiles.is_empty() {
            drop(packfiles);
            self.resume().await?;
            packfiles = self.packfile.read().await;
        }

        let mut packfile = {
            let writeable = self.writeable.read().await;

            if writeable.is_empty() {
                return Err(PackfileError::internal("No more packfiles"));
            }

            let mut writeable_index = (size + buffer.as_ptr() as usize) % writeable.len();
            let mut retry = 0;

            let mut packfile_id = writeable[writeable_index];
            // Writeable IDs are guaranteed to be valid since packfile list never shrinks
            let mut packfile_index = (packfile_id - 1) as usize;
            let mut try_lock = packfiles[packfile_index].try_write();
            while try_lock.is_err() {
                retry += 1;
                writeable_index = (writeable_index + 1) % writeable.len();
                if retry >= writeable.len() {
                    break;
                }
                packfile_id = writeable[writeable_index];
                packfile_index = (packfile_id - 1) as usize;
                try_lock = packfiles[packfile_index].try_write();
            }

            drop(writeable);

            match try_lock {
                Ok(guard) => guard,
                Err(_) => packfiles[packfile_index].write().await,
            }
        };

        let offset = packfile.size;
        let id = packfile.id;
        let mut full = false;

        packfile.dirty.store(true, Ordering::Relaxed);

        if let Some(file) = packfile.file.as_ref() {
            packfile_write(file, buffer, offset).await?;
            packfile.size += size;
            if packfile.size >= PACKSTORE_SIZE_LIMIT as usize {
                full = true;
            }
        } else {
            if (packfile.buffer.len() + size) > packfile.buffer.capacity() {
                packfile.buffer.reserve(4 * 1024 * 1024);
            }

            packfile.buffer.extend_from_slice(&buffer[..size]);
            packfile.size += size;
        }

        drop(packfile);
        drop(packfiles);

        if full {
            self.mark_full(id).await;
        }

        Ok(PackStoreRef {
            id,
            offset: offset as u32,
        })
    }

    pub async fn obliterate(&self, id: u32, offset: u32, size: u32) -> Result<(), PackfileError> {
        let offset = offset as usize;
        let size = size as usize;
        let zeros = BytesMut::zeroed(size).freeze();

        let mut packfiles = self.packfile.read().await;
        if packfiles.is_empty() {
            drop(packfiles);
            self.resume().await?;
            packfiles = self.packfile.read().await;
        }

        if id == 0 || id as usize > packfiles.len() {
            return Err(PackfileError::internal("Invalid packfile"));
        }

        let index = (id - 1) as usize;
        let packfile = packfiles[index].write().await;
        if packfile.id != id {
            return Err(PackfileError::internal("Packfile ID mismatch index"));
        }

        packfile.dirty.store(true, Ordering::Relaxed);

        if let Some(file) = packfile.file.as_ref() {
            packfile_write(file, zeros, offset).await?;
            let _ = file.sync_data().await;
        }

        Ok(())
    }

    pub async fn flush(&self, id: u32, sync_data: bool) -> Result<(), PackfileError> {
        let packfile = self.packfile.read().await;

        if id == 0 || id as usize > packfile.len() {
            return Err(PackfileError::internal("Invalid packfile"));
        }

        let index = (id - 1) as usize;
        let packfile = packfile[index].read().await;
        if packfile.id != id {
            return Err(PackfileError::internal("Packfile ID mismatch index"));
        }

        if packfile
            .dirty
            .compare_exchange(true, false, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return Ok(());
        }

        if sync_data && let Some(file) = &packfile.file {
            let _ = file.sync_data().await;
        }

        Ok(())
    }

    pub async fn flush_all(&self, sync_data: bool) {
        let packfile = self.packfile.read().await;

        let mut flush_tasks = JoinSet::new();
        for packfile in packfile.iter() {
            let packfile = packfile.read().await;

            if packfile
                .dirty
                .compare_exchange(true, false, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }

            if sync_data && let Some(file) = &packfile.file {
                let file = file.clone();
                lore_base::lore_spawn!(flush_tasks, async move {
                    let _ = file.sync_data().await;
                });
            }
        }

        while let Some(_result) = flush_tasks.join_next().await {}
    }

    pub async fn total_size(&self) -> Result<usize, PackfileError> {
        let mut size = 0;
        let mut packfiles = self.packfile.read().await;
        if packfiles.is_empty() {
            drop(packfiles);
            self.resume().await?;
            packfiles = self.packfile.read().await;
        }
        for packfile in packfiles.iter() {
            size += packfile.read().await.size;
        }
        Ok(size)
    }
}
