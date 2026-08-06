// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;

#[cfg(not(target_family = "windows"))]
pub fn rename_file<P: AsRef<Path>>(from: P, to: P) -> std::io::Result<()> {
    std::fs::rename(from.as_ref(), to.as_ref())
}

#[cfg(target_family = "windows")]
pub fn rename_file<P: AsRef<Path>>(from: P, to: P) -> std::io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::*;

    // `to_extended_wide` applies the \\?\ verbatim prefix only when the
    // path would otherwise exceed MAX_PATH, so short paths skip the prefix
    // overhead. MoveFileExW parses each parameter independently, so a
    // short non-prefixed source and a long prefixed destination (or any
    // mix) resolve correctly.
    let from = lore_base::fs::win_path::to_extended_wide(from.as_ref());
    let to = lore_base::fs::win_path::to_extended_wide(to.as_ref());

    // Safety: Call Win32 APIs, buffers are valid and null-terminated
    let ok = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_REPLACE_EXISTING) };

    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(target_family = "windows"))]
pub fn sync_dir<P: AsRef<Path>>(path: P) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    let dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(path.as_ref())?;
    let fd = dir.as_raw_fd();
    // SAFETY: Safe to call libc function to flush directory changes
    let result = unsafe { libc::fsync(fd) };
    if result == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(target_family = "windows")]
pub fn sync_dir<P: AsRef<Path>>(_path: P) -> std::io::Result<()> {
    // No-op on Windows, there is no API to flush a directory
    Ok(())
}

pub async fn unlink_recursive<P: AsRef<Path>>(absolute_path: P) -> tokio::io::Result<()> {
    let absolute_path = absolute_path.as_ref();
    lore_base::lore_trace!("Deleting {}", absolute_path.display());
    let driver = lore_io::IoDriver::global();
    let metadata = match driver.metadata(absolute_path).await {
        Ok(metadata) => metadata,
        // Nothing to delete is a delete that has already happened. Any other failure — a
        // permission error on the parent, for one — is reported rather than read as absence,
        // which would report a successful delete of something still there.
        Err(err) if err.kind() == tokio::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };

    if metadata.is_dir() {
        if let Err(err) = driver.remove_dir_all(absolute_path).await {
            if err.kind() == tokio::io::ErrorKind::NotFound {
                return Ok(());
            }
            lore_base::lore_debug!(
                "Error deleting directory {}: {} - retry after setting write permission",
                absolute_path.display(),
                err
            );

            let mut permissions = metadata.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            // Blocking on purpose: the driver has no permissions operation, and this runs only
            // after a delete has already failed, so it is bounded to the retry path.
            let _ = std::fs::set_permissions(absolute_path, permissions);
            if let Err(err) = driver.remove_dir_all(absolute_path).await {
                if err.kind() == tokio::io::ErrorKind::NotFound {
                    return Ok(());
                }
                return Err(err);
            }
        }
    } else if let Err(err) = driver.remove_file(absolute_path).await {
        if err.kind() == tokio::io::ErrorKind::NotFound {
            return Ok(());
        }

        let mut permissions = metadata.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        // Blocking for the same reason as the directory retry above.
        let _ = std::fs::set_permissions(absolute_path, permissions);
        if let Err(err) = driver.remove_file(absolute_path).await {
            if err.kind() == tokio::io::ErrorKind::NotFound {
                return Ok(());
            }
            return Err(err);
        }
    }

    Ok(())
}
