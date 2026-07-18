// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Scan reconciliation for reverted unstaged adds.
//!
//! Deleting a never-committed file or folder from disk must remove its
//! dirty-add tracking entirely: a follow-up `status --scan` must not report
//! the vanished paths at all. Only content that exists in the current
//! revision may be reported as `Delete`.
mod test_util;

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::str::FromStr;
    use std::sync::Arc;

    use lore::file::LoreFileStageArgs;
    use lore::repository::LoreRepositoryCreateArgs;
    use lore::repository::LoreRepositoryStatusArgs;
    use lore::revision::LoreRevisionCommitArgs;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreEvent;
    use lore_revision::interface::LoreFileAction;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreString;
    use parking_lot::Mutex;
    use rand::distr::Alphanumeric;
    use rand::distr::SampleString;

    use super::test_util::TempDir;

    fn offline_globals(repository_path: &std::path::Path) -> LoreGlobalArgs {
        LoreGlobalArgs {
            repository_path: repository_path.into(),
            offline: 1,
            identity: "test-user".into(),
            ..Default::default()
        }
    }

    async fn create_repository(globals: &LoreGlobalArgs) {
        let name: String = Alphanumeric.sample_string(&mut rand::rng(), 16);
        let mut url = String::from_str("lore://localhost/").unwrap_or_default();
        url.push_str(name.as_str());
        let args = LoreRepositoryCreateArgs {
            repository_url: url.into(),
            id: LoreString::default(),
            description: LoreString::default(),
            use_shared_store: 0,
            shared_store_path: LoreString::default(),
        };
        let result = lore::repository::create(globals.clone(), args, None).await;
        assert_eq!(result, 0, "Failed to create repository");
    }

    fn write_file(path: &std::path::Path, content: &[u8]) {
        std::fs::create_dir_all(path.parent().expect("file has a parent"))
            .expect("Failed to create parent directory");
        let mut file = std::fs::File::options()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)
            .expect("Failed to create test file");
        file.write_all(content).expect("Failed to write test file");
    }

    async fn stage_and_commit(
        globals: &LoreGlobalArgs,
        paths: Vec<LoreString>,
        message: &str,
    ) {
        let args = LoreFileStageArgs {
            paths: LoreArray::from_vec(paths),
            case_change: 0,
            scan: 1,
        };
        let result = lore::file::stage(globals.clone(), args, None).await;
        assert_eq!(result, 0, "Failed to stage files");

        let args = LoreRevisionCommitArgs {
            message: LoreString::from(message),
            ..Default::default()
        };
        let result = lore::revision::commit(globals.clone(), args, None).await;
        assert_eq!(result, 0, "Failed to commit");
    }

    /// Run `status --scan` and collect the reported (path, action) pairs with
    /// paths normalized to forward slashes.
    async fn scan_status(globals: &LoreGlobalArgs) -> Vec<(String, LoreFileAction)> {
        let entries: Arc<Mutex<Vec<(String, LoreFileAction)>>> = Arc::new(Mutex::new(Vec::new()));
        let entries_ = Arc::clone(&entries);
        let status_ok: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        let status_ok_ = Arc::clone(&status_ok);

        let callback = Some(Box::new(move |event: &LoreEvent| match event {
            LoreEvent::RepositoryStatusFile(data) => {
                entries_
                    .lock()
                    .push((data.path.as_str().replace('\\', "/"), data.action));
            }
            LoreEvent::Complete(data) => {
                *status_ok_.lock() = data.status == 0;
            }
            LoreEvent::Error(data) => {
                eprintln!("Error {}: {}", data.error_type, data.error_inner.as_str());
            }
            _ => (),
        }) as Box<_>);

        let args = LoreRepositoryStatusArgs {
            staged: 1,
            scan: 1,
            check_dirty: 0,
            reset: 0,
            sync_point: 0,
            revision_only: 0,
            count: 0,
            paths: LoreArray::default(),
        };
        let result = lore::repository::status(globals.clone(), args, callback).await;
        assert_eq!(result, 0, "Status call failed");
        assert!(*status_ok.lock(), "Status did not complete successfully");

        let collected = entries.lock().clone();
        collected
    }

    fn entries_under<'a>(
        entries: &'a [(String, LoreFileAction)],
        prefix: &str,
    ) -> Vec<&'a (String, LoreFileAction)> {
        entries
            .iter()
            .filter(|(path, _)| {
                let trimmed = path.trim_end_matches('/');
                trimmed == prefix || path.starts_with(&format!("{prefix}/"))
            })
            .collect()
    }

    /// Deleting a never-committed folder must remove every trace of it from
    /// the scan: no `Delete` entries for paths that were never part of a
    /// revision, on this scan or any later one.
    #[tokio::test]
    async fn scan_forgets_deleted_untracked_folder() {
        let tempdir = TempDir::new("lore-scan-revert-test-");
        let repository_path = tempdir.path().to_path_buf();
        let globals = offline_globals(&repository_path);

        create_repository(&globals).await;

        // A committed baseline so the repository has a non-empty revision.
        let base = repository_path.join("base.txt");
        write_file(&base, b"base");
        stage_and_commit(&globals, vec![LoreString::from(&base)], "base").await;

        // Untracked folder with nested content, registered by a scan.
        write_file(&repository_path.join("newfolder/one.png"), b"one");
        write_file(&repository_path.join("newfolder/two.png"), b"two");
        write_file(&repository_path.join("newfolder/sub/three.png"), b"three");

        let entries = scan_status(&globals).await;
        let added = entries_under(&entries, "newfolder");
        assert!(
            added
                .iter()
                .any(|(path, action)| path.ends_with("one.png") && *action == LoreFileAction::Add),
            "expected newfolder/one.png reported as Add, got: {entries:?}"
        );

        // Revert the add by deleting the folder from disk.
        std::fs::remove_dir_all(repository_path.join("newfolder"))
            .expect("Failed to remove newfolder");

        let entries = scan_status(&globals).await;
        assert!(
            entries_under(&entries, "newfolder").is_empty(),
            "vanished never-committed folder must not be reported, got: {entries:?}"
        );

        // The staged state must be clean of the folder too: a second scan must
        // not resurrect phantom entries from leftover nodes or dirty flags.
        let entries = scan_status(&globals).await;
        assert!(
            entries_under(&entries, "newfolder").is_empty(),
            "phantom entries resurfaced on a later scan: {entries:?}"
        );
    }

    /// Deleting a never-committed EMPTY folder must leave no trace either —
    /// whether or not the scan registered a node for it, later scans must not
    /// report the path.
    #[tokio::test]
    async fn scan_forgets_deleted_untracked_empty_folder() {
        let tempdir = TempDir::new("lore-scan-revert-test-");
        let repository_path = tempdir.path().to_path_buf();
        let globals = offline_globals(&repository_path);

        create_repository(&globals).await;

        let base = repository_path.join("base.txt");
        write_file(&base, b"base");
        stage_and_commit(&globals, vec![LoreString::from(&base)], "base").await;

        std::fs::create_dir_all(repository_path.join("emptyfolder"))
            .expect("Failed to create emptyfolder");
        scan_status(&globals).await;

        std::fs::remove_dir_all(repository_path.join("emptyfolder"))
            .expect("Failed to remove emptyfolder");

        let entries = scan_status(&globals).await;
        assert!(
            entries_under(&entries, "emptyfolder").is_empty(),
            "vanished never-committed empty folder must not be reported, got: {entries:?}"
        );
        let entries = scan_status(&globals).await;
        assert!(
            entries_under(&entries, "emptyfolder").is_empty(),
            "phantom empty folder resurfaced on a later scan: {entries:?}"
        );
    }

    /// Deleting a committed folder must still report `Delete` for its content.
    #[tokio::test]
    async fn scan_reports_deletes_for_committed_folder() {
        let tempdir = TempDir::new("lore-scan-revert-test-");
        let repository_path = tempdir.path().to_path_buf();
        let globals = offline_globals(&repository_path);

        create_repository(&globals).await;

        let file_a = repository_path.join("folder/a.txt");
        let file_b = repository_path.join("folder/b.txt");
        write_file(&file_a, b"a");
        write_file(&file_b, b"b");
        stage_and_commit(
            &globals,
            vec![LoreString::from(&file_a), LoreString::from(&file_b)],
            "add folder",
        )
        .await;

        std::fs::remove_dir_all(repository_path.join("folder")).expect("Failed to remove folder");

        let entries = scan_status(&globals).await;
        for file in ["folder/a.txt", "folder/b.txt"] {
            assert!(
                entries
                    .iter()
                    .any(|(path, action)| path == file && *action == LoreFileAction::Delete),
                "expected {file} reported as Delete, got: {entries:?}"
            );
        }
    }

    /// Deleting a committed folder that also contains never-committed files
    /// must report `Delete` only for the committed content.
    #[tokio::test]
    async fn scan_mixed_folder_reports_only_committed_deletes() {
        let tempdir = TempDir::new("lore-scan-revert-test-");
        let repository_path = tempdir.path().to_path_buf();
        let globals = offline_globals(&repository_path);

        create_repository(&globals).await;

        let committed = repository_path.join("folder/committed.txt");
        write_file(&committed, b"committed");
        stage_and_commit(&globals, vec![LoreString::from(&committed)], "add folder").await;

        // Drop an untracked file into the committed folder and register it.
        write_file(&repository_path.join("folder/untracked.txt"), b"untracked");
        let entries = scan_status(&globals).await;
        assert!(
            entries
                .iter()
                .any(|(path, action)| path == "folder/untracked.txt"
                    && *action == LoreFileAction::Add),
            "expected folder/untracked.txt reported as Add, got: {entries:?}"
        );

        std::fs::remove_dir_all(repository_path.join("folder")).expect("Failed to remove folder");

        let entries = scan_status(&globals).await;
        assert!(
            entries
                .iter()
                .any(|(path, action)| path == "folder/committed.txt"
                    && *action == LoreFileAction::Delete),
            "expected folder/committed.txt reported as Delete, got: {entries:?}"
        );
        assert!(
            !entries
                .iter()
                .any(|(path, _)| path == "folder/untracked.txt"),
            "vanished never-committed file must not be reported, got: {entries:?}"
        );

        // Later scans stay clean as well.
        let entries = scan_status(&globals).await;
        assert!(
            !entries
                .iter()
                .any(|(path, _)| path == "folder/untracked.txt"),
            "phantom entry resurfaced on a later scan: {entries:?}"
        );
    }
}
