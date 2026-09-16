// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use std::path::Path;
use std::path::PathBuf;

use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

use crate::LocalImmutableStoreError;

/// Magic value at the start of the info file. Bytes spell `IS_I` on a little-endian target.
const INFO_MAGIC: u32 = u32::from_le_bytes(*b"IS_I");

/// Bumped only if the file's binary layout changes.
#[repr(u32)]
enum ImmutableStoreInfoVersion {
    Initial = 0,
}

#[repr(C)]
#[derive(Debug, IntoBytes, FromBytes, Immutable)]
pub struct ImmutableStoreInfo {
    magic: u32,
    version: u32,

    /// What is the next group that needs migrated from local Oodle data?
    /// -1 is if there is no more migration to be done.
    ///
    /// Descending: the number of materialized groups can only increase with store usage,
    /// and groups that are newly materialized after this index
    /// are known to never contain Oodle since it is no longer allowed by `lore_storage`.
    /// Therefore, groups are migrated from N down.
    pub next_group_index_to_migrate_oodle: i32,
}

impl Default for ImmutableStoreInfo {
    fn default() -> Self {
        Self {
            magic: INFO_MAGIC,
            version: ImmutableStoreInfoVersion::Initial as u32,
            next_group_index_to_migrate_oodle: -1,
        }
    }
}

struct ImmutableStoreInfoSegment(Box<[u8; size_of::<ImmutableStoreInfo>()]>);

impl lore_io::StableBufList for ImmutableStoreInfoSegment {
    fn byte_segments(&self) -> impl Iterator<Item = &[u8]> {
        std::iter::once(self.0.as_ref().as_slice())
    }
}

pub fn info_path_for_store_root(path: &Path) -> PathBuf {
    path.join("info")
}

pub async fn read_info_file(path: &Path) -> std::io::Result<Option<ImmutableStoreInfo>> {
    let bytes = match lore_io::IoDriver::global().read_file_bytes(path).await {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let mut header = ImmutableStoreInfo::new_zeroed();
    let expected = size_of::<ImmutableStoreInfo>();
    if bytes.len() < expected {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("Info file is {} bytes, expected {expected}", bytes.len()),
        ));
    }
    header.as_mut_bytes().copy_from_slice(&bytes[..expected]);
    if header.magic != INFO_MAGIC {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "Info file has invalid magic 0x{:08x}, expected 0x{:08x}",
                header.magic, INFO_MAGIC
            ),
        ));
    }
    if header.version != ImmutableStoreInfoVersion::Initial as u32 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "Info file has unsupported version {}, expected {}",
                header.version,
                ImmutableStoreInfoVersion::Initial as u32
            ),
        ));
    }

    Ok(Some(header))
}

/// Replace the info file at `path`. The write is atomic and durable: a torn one would leave a
/// file that cannot be read, and the store cannot be opened without it.
pub async fn write_info_file(info: &ImmutableStoreInfo, path: &Path) -> std::io::Result<()> {
    let mut segment = Box::new([0u8; size_of::<ImmutableStoreInfo>()]);
    segment.copy_from_slice(info.as_bytes());
    lore_io::IoDriver::global()
        .write_file_segments_atomic(
            path.with_extension("tmp"),
            path,
            &lore_io::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true),
            ImmutableStoreInfoSegment(segment),
        )
        .await?;
    Ok(())
}

pub async fn get_or_init_disk_info(
    path: &Path,
    next_group_index_to_migrate_oodle: i32,
) -> Result<ImmutableStoreInfo, LocalImmutableStoreError> {
    let info_file_path = info_path_for_store_root(path);

    if let Some(info_from_disk) = read_info_file(info_file_path.as_path())
        .await
        .map_err(|err| {
            LocalImmutableStoreError::internal_with_context(err, "Failed to load store info file")
        })?
    {
        return Ok(info_from_disk);
    }

    let mut new_info = ImmutableStoreInfo::default();
    if next_group_index_to_migrate_oodle != -1 {
        new_info.next_group_index_to_migrate_oodle = next_group_index_to_migrate_oodle;
    }
    write_info_file(&new_info, info_file_path.as_path())
        .await
        .map_err(|err| {
            LocalImmutableStoreError::internal_with_context(
                err,
                "Failed to store initial info file",
            )
        })?;

    Ok(new_info)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO_SIZE: usize = size_of::<ImmutableStoreInfo>();

    fn info_at(next_group_index_to_migrate_oodle: i32) -> ImmutableStoreInfo {
        ImmutableStoreInfo {
            next_group_index_to_migrate_oodle,
            ..Default::default()
        }
    }

    /// Bytes of a valid info file, with `corrupt` applied before they are written.
    fn corrupted_bytes(corrupt: impl FnOnce(&mut [u8; INFO_SIZE])) -> [u8; INFO_SIZE] {
        let mut bytes = [0u8; INFO_SIZE];
        bytes.copy_from_slice(info_at(7).as_bytes());
        corrupt(&mut bytes);
        bytes
    }

    mod read_info_file {
        use super::*;

        #[tokio::test]
        async fn an_absent_file_reads_as_no_info() {
            let dir = crate::test_util::TempDir::new("is_info_absent_");
            let info = read_info_file(&info_path_for_store_root(dir.path()))
                .await
                .expect("an absent info file is not a failure");
            assert!(info.is_none());
        }

        #[tokio::test]
        async fn a_written_file_reads_back_unchanged() {
            let dir = crate::test_util::TempDir::new("is_info_round_trip_");
            let path = info_path_for_store_root(dir.path());

            write_info_file(&info_at(42), &path)
                .await
                .expect("info file writes");

            let info = read_info_file(&path)
                .await
                .expect("info file reads")
                .expect("info file is present");
            assert_eq!(info.next_group_index_to_migrate_oodle, 42);
            assert_eq!(info.magic, INFO_MAGIC);
            assert_eq!(info.version, ImmutableStoreInfoVersion::Initial as u32);
        }

        #[tokio::test]
        async fn a_truncated_file_is_rejected() {
            let dir = crate::test_util::TempDir::new("is_info_short_");
            let path = info_path_for_store_root(dir.path());
            std::fs::write(&path, &corrupted_bytes(|_| {})[..INFO_SIZE - 1])
                .expect("short file writes");

            let err = read_info_file(&path)
                .await
                .expect_err("a file too short to hold the header is rejected");
            assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        }

        #[tokio::test]
        async fn a_foreign_magic_is_rejected() {
            let dir = crate::test_util::TempDir::new("is_info_magic_");
            let path = info_path_for_store_root(dir.path());
            let bytes = corrupted_bytes(|bytes| bytes[..4].copy_from_slice(b"XXXX"));
            std::fs::write(&path, bytes).expect("file writes");

            let err = read_info_file(&path)
                .await
                .expect_err("a file that is not an info file is rejected");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        }

        #[tokio::test]
        async fn an_unsupported_version_is_rejected() {
            let dir = crate::test_util::TempDir::new("is_info_version_");
            let path = info_path_for_store_root(dir.path());
            let unsupported = ImmutableStoreInfoVersion::Initial as u32 + 1;
            let bytes =
                corrupted_bytes(|bytes| bytes[4..8].copy_from_slice(&unsupported.to_ne_bytes()));
            std::fs::write(&path, bytes).expect("file writes");

            let err = read_info_file(&path)
                .await
                .expect_err("a layout this binary cannot parse is rejected");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        }
    }

    mod write_info_file {
        use super::*;

        #[tokio::test]
        async fn a_rewrite_leaves_no_trailing_bytes_of_the_previous_file() {
            let dir = crate::test_util::TempDir::new("is_info_rewrite_");
            let path = info_path_for_store_root(dir.path());
            std::fs::write(&path, [0xAB; INFO_SIZE * 4]).expect("oversized file writes");

            write_info_file(&info_at(3), &path)
                .await
                .expect("info file writes");

            assert_eq!(
                std::fs::metadata(&path).expect("info file exists").len(),
                INFO_SIZE as u64
            );
            assert!(
                !path.with_extension("tmp").exists(),
                "the file the atomic write renames from must not outlive it"
            );
            let info = read_info_file(&path)
                .await
                .expect("info file reads")
                .expect("info file is present");
            assert_eq!(info.next_group_index_to_migrate_oodle, 3);
        }
    }

    /// The on-disk layout, which `#[repr(C)]` is what fixes. A field moving would put something
    /// other than the magic at offset 0, and every info file already written would stop parsing.
    #[test]
    fn the_magic_leads_the_serialized_form() {
        let info = info_at(7);
        let bytes = info.as_bytes();
        assert_eq!(bytes.len(), INFO_SIZE);
        assert_eq!(bytes[..4], INFO_MAGIC.to_ne_bytes());
        assert_eq!(
            bytes[8..12],
            7i32.to_ne_bytes(),
            "the resume index trails the magic and version"
        );
    }

    mod get_or_init_disk_info {
        use super::*;

        #[tokio::test]
        async fn a_store_with_no_info_file_records_the_seed() {
            let dir = crate::test_util::TempDir::new("is_info_init_seed_");
            let info = get_or_init_disk_info(dir.path(), 17)
                .await
                .expect("info file initialises");
            assert_eq!(info.next_group_index_to_migrate_oodle, 17);

            let on_disk = read_info_file(&info_path_for_store_root(dir.path()))
                .await
                .expect("info file reads")
                .expect("the seed was persisted, not just returned");
            assert_eq!(on_disk.next_group_index_to_migrate_oodle, 17);
        }

        /// A store with no written group has nothing to migrate, and the seed says so.
        #[tokio::test]
        async fn a_seed_of_minus_one_leaves_nothing_to_migrate() {
            let dir = crate::test_util::TempDir::new("is_info_init_empty_");
            let info = get_or_init_disk_info(dir.path(), -1)
                .await
                .expect("info file initialises");
            assert_eq!(info.next_group_index_to_migrate_oodle, -1);
        }

        /// The seed describes the store as it was first seen. Once progress is on disk it is
        /// authoritative, or a pass would restart from the top on every open.
        #[tokio::test]
        async fn an_existing_file_wins_over_the_seed() {
            let dir = crate::test_util::TempDir::new("is_info_init_existing_");
            write_info_file(&info_at(5), &info_path_for_store_root(dir.path()))
                .await
                .expect("info file writes");

            let info = get_or_init_disk_info(dir.path(), 200)
                .await
                .expect("info file loads");
            assert_eq!(info.next_group_index_to_migrate_oodle, 5);
        }

        #[tokio::test]
        async fn a_corrupt_file_is_reported_rather_than_replaced() {
            let dir = crate::test_util::TempDir::new("is_info_init_corrupt_");
            let path = info_path_for_store_root(dir.path());
            let bytes = corrupted_bytes(|bytes| bytes[..4].copy_from_slice(b"XXXX"));
            std::fs::write(&path, bytes).expect("file writes");

            let err = get_or_init_disk_info(dir.path(), 9)
                .await
                .expect_err("a corrupt info file is not silently overwritten");
            assert!(err.is_internal());

            let after = std::fs::read(&path).expect("info file still exists");
            assert_eq!(&after[..4], b"XXXX", "the corrupt file was left in place");
        }
    }
}
