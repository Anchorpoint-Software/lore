// SPDX-FileCopyrightText: 2026 Anchorpoint Software GmbH
// SPDX-License-Identifier: MIT
mod test_util;

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::Mutex;

    use lore::file::LoreFileInfoArgs;
    use lore::file::LoreFileReadArgs;
    use lore::file::LoreFileStageArgs;
    use lore::repository::LoreRepositoryCreateArgs;
    use lore::revision::LoreRevisionCommitArgs;
    use lore_revision::event::LoreEvent;
    use lore_revision::interface::LoreArray;
    use lore_revision::interface::LoreEventCallback;
    use lore_revision::interface::LoreGlobalArgs;
    use lore_revision::interface::LoreString;
    use rand::Rng;
    use rand::distr::Alphanumeric;

    use super::test_util::TempDir;

    fn globals(repo: &std::path::Path) -> LoreGlobalArgs {
        LoreGlobalArgs {
            repository_path: repo.into(),
            offline: 1,
            identity: "test-user".into(),
            ..Default::default()
        }
    }

    async fn create_repo(globals: &LoreGlobalArgs) {
        let name: String = rand::rng()
            .sample_iter(&Alphanumeric)
            .take(16)
            .map(char::from)
            .collect();
        let args = LoreRepositoryCreateArgs {
            repository_url: format!("lore://localhost/{name}").into(),
            id: LoreString::default(),
            description: LoreString::default(),
            use_shared_store: 0,
            shared_store_path: LoreString::default(),
        };
        assert_eq!(
            lore::repository::create(globals.clone(), args, None).await,
            0,
            "Failed to create repository"
        );
    }

    /// Write `payload` to `file_path`, stage and commit it; returns the new
    /// revision signature.
    async fn commit_file(
        globals: &LoreGlobalArgs,
        file_path: &std::path::Path,
        payload: &[u8],
    ) -> String {
        {
            let mut file = std::fs::File::options()
                .create(true)
                .truncate(true)
                .write(true)
                .open(file_path)
                .expect("create payload file");
            file.write_all(payload).expect("write payload file");
        }
        let args = LoreFileStageArgs {
            paths: LoreArray::from_vec(vec![LoreString::from(file_path)]),
            case_change: 0,
            scan: 0,
        };
        assert_eq!(
            lore::file::stage(globals.clone(), args, None).await,
            0,
            "stage failed"
        );

        let sig: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let sink = sig.clone();
        let cb: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::RevisionCommitRevision(d) = event {
                *sink.lock().unwrap() = Some(d.revision.to_string());
            }
        }));
        let args = LoreRevisionCommitArgs {
            message: LoreString::from("commit"),
            ..Default::default()
        };
        assert_eq!(
            lore::revision::commit(globals.clone(), args, cb).await,
            0,
            "commit failed"
        );
        sig.lock()
            .unwrap()
            .clone()
            .expect("commit emitted no revision")
    }

    async fn file_address(globals: &LoreGlobalArgs, file_path: &std::path::Path) -> String {
        let addr: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let sink = addr.clone();
        let cb: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::FileInfo(d) = event {
                *sink.lock().unwrap() = Some(format!("{}-{}", d.hash, d.context));
            }
        }));
        let args = LoreFileInfoArgs {
            paths: LoreArray::from_vec(vec![LoreString::from(file_path)]),
            revision: LoreString::default(),
            local: 0,
            filtered: 0,
        };
        assert_eq!(
            lore::file::info(globals.clone(), args, cb).await,
            0,
            "info failed"
        );
        addr.lock()
            .unwrap()
            .clone()
            .expect("info emitted no address")
    }

    /// Concatenate the bytes delivered across the `FileRead` events, copying out
    /// of the borrowed view inside the callback.
    fn collect_callback() -> (Arc<Mutex<Vec<u8>>>, LoreEventCallback) {
        let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = buf.clone();
        let callback: LoreEventCallback = Some(Box::new(move |event: &LoreEvent| {
            if let LoreEvent::FileRead(data) = event
                && data.bytes.len > 0
            {
                let slice = unsafe {
                    std::slice::from_raw_parts(data.bytes.ptr.cast::<u8>(), data.bytes.len)
                };
                sink.lock().unwrap().extend_from_slice(slice);
            }
        }));
        (buf, callback)
    }

    async fn read_path(globals: &LoreGlobalArgs, file_path: &std::path::Path) -> (i32, Vec<u8>) {
        let (buf, callback) = collect_callback();
        let args = LoreFileReadArgs {
            address: LoreString::default(),
            path: file_path.into(),
            revision: LoreString::default(),
        };
        let status = lore::file::read(globals.clone(), args, callback).await;
        let bytes = buf.lock().unwrap().clone();
        (status, bytes)
    }

    #[tokio::test]
    async fn read_small_file_returns_committed_bytes() {
        let dir = TempDir::new("lore-file-read-small-");
        let g = globals(dir.path());
        create_repo(&g).await;
        let mut payload = [0u8; 1024];
        rand::rng().fill(&mut payload[..]);
        let file = dir.path().join("small.bin");
        commit_file(&g, &file, &payload).await;

        let (status, bytes) = read_path(&g, &file).await;
        assert_eq!(status, 0, "read failed");
        assert_eq!(bytes.as_slice(), &payload, "small read content mismatch");
    }

    #[tokio::test]
    async fn read_large_file_streams_committed_bytes() {
        let dir = TempDir::new("lore-file-read-large-");
        let g = globals(dir.path());
        create_repo(&g).await;
        let mut payload = vec![0u8; 3 * 1024 * 1024];
        rand::rng().fill(&mut payload[..]);
        let file = dir.path().join("large.bin");
        commit_file(&g, &file, &payload).await;

        let (status, bytes) = read_path(&g, &file).await;
        assert_eq!(status, 0, "read failed");
        assert_eq!(bytes.len(), payload.len(), "large read length mismatch");
        assert_eq!(bytes, payload, "large read content mismatch");
    }

    #[tokio::test]
    async fn read_by_address_returns_committed_bytes() {
        let dir = TempDir::new("lore-file-read-addr-");
        let g = globals(dir.path());
        create_repo(&g).await;
        let mut payload = [0u8; 2048];
        rand::rng().fill(&mut payload[..]);
        let file = dir.path().join("byaddr.bin");
        commit_file(&g, &file, &payload).await;
        let address = file_address(&g, &file).await;

        let (buf, callback) = collect_callback();
        let args = LoreFileReadArgs {
            address: LoreString::from(address.as_str()),
            path: LoreString::default(),
            revision: LoreString::default(),
        };
        assert_eq!(
            lore::file::read(g.clone(), args, callback).await,
            0,
            "read by address failed"
        );
        assert_eq!(
            buf.lock().unwrap().as_slice(),
            &payload,
            "by-address content mismatch"
        );
    }

    #[tokio::test]
    async fn read_at_older_revision_returns_old_content() {
        let dir = TempDir::new("lore-file-read-rev-");
        let g = globals(dir.path());
        create_repo(&g).await;
        let file = dir.path().join("versioned.bin");
        let v1 = b"first version contents".to_vec();
        let sig1 = commit_file(&g, &file, &v1).await;
        let v2 = b"second version, a different length of contents".to_vec();
        commit_file(&g, &file, &v2).await;

        let (status, head) = read_path(&g, &file).await;
        assert_eq!(status, 0);
        assert_eq!(head.as_slice(), v2.as_slice(), "head should be v2");

        let (buf, callback) = collect_callback();
        let args = LoreFileReadArgs {
            address: LoreString::default(),
            path: file.as_path().into(),
            revision: LoreString::from(sig1.as_str()),
        };
        assert_eq!(
            lore::file::read(g.clone(), args, callback).await,
            0,
            "read at older revision failed"
        );
        assert_eq!(
            buf.lock().unwrap().as_slice(),
            v1.as_slice(),
            "older revision should be v1"
        );
    }

    #[tokio::test]
    async fn read_missing_file_fails() {
        let dir = TempDir::new("lore-file-read-missing-");
        let g = globals(dir.path());
        create_repo(&g).await;
        commit_file(&g, &dir.path().join("present.bin"), b"present").await;

        let (status, bytes) = read_path(&g, &dir.path().join("does-not-exist.bin")).await;
        assert_ne!(status, 0, "reading a missing file must fail");
        assert!(bytes.is_empty(), "no content for a missing file");
    }

    #[tokio::test]
    async fn read_zero_size_file_succeeds_empty() {
        let dir = TempDir::new("lore-file-read-empty-");
        let g = globals(dir.path());
        create_repo(&g).await;
        let file = dir.path().join("empty.bin");
        commit_file(&g, &file, b"").await;

        let (status, bytes) = read_path(&g, &file).await;
        assert_eq!(status, 0, "zero-size read should succeed");
        assert!(bytes.is_empty(), "zero-size read should yield no bytes");
    }

    #[tokio::test]
    async fn read_invalid_address_fails() {
        let dir = TempDir::new("lore-file-read-badaddr-");
        let g = globals(dir.path());
        create_repo(&g).await;

        let (buf, callback) = collect_callback();
        let args = LoreFileReadArgs {
            address: LoreString::from("not-an-address"),
            path: LoreString::default(),
            revision: LoreString::default(),
        };
        let status = lore::file::read(g.clone(), args, callback).await;
        assert_ne!(status, 0, "a malformed address must fail");
        assert!(
            buf.lock().unwrap().is_empty(),
            "no content for a malformed address"
        );
    }

    #[tokio::test]
    async fn read_directory_path_fails() {
        let dir = TempDir::new("lore-file-read-dir-");
        let g = globals(dir.path());
        create_repo(&g).await;
        let subdir = dir.path().join("sub");
        std::fs::create_dir_all(&subdir).expect("create subdir");
        commit_file(&g, &subdir.join("inner.bin"), b"inner contents").await;

        let (status, bytes) = read_path(&g, &subdir).await;
        assert_ne!(status, 0, "reading a directory path must fail");
        assert!(bytes.is_empty(), "no content for a directory path");
    }

    #[tokio::test]
    async fn read_unknown_revision_fails() {
        let dir = TempDir::new("lore-file-read-badrev-");
        let g = globals(dir.path());
        create_repo(&g).await;
        let file = dir.path().join("present.bin");
        commit_file(&g, &file, b"present contents").await;

        let (buf, callback) = collect_callback();
        let args = LoreFileReadArgs {
            address: LoreString::default(),
            path: file.as_path().into(),
            revision: LoreString::from("deadbeef"),
        };
        let status = lore::file::read(g.clone(), args, callback).await;
        assert_ne!(status, 0, "an unknown revision must fail");
        assert!(
            buf.lock().unwrap().is_empty(),
            "no content for an unknown revision"
        );
    }

    #[tokio::test]
    async fn read_unknown_address_fails() {
        let dir = TempDir::new("lore-file-read-missaddr-");
        let g = globals(dir.path());
        create_repo(&g).await;
        let file = dir.path().join("real.bin");
        commit_file(&g, &file, b"real contents").await;
        let address = file_address(&g, &file).await;

        // Flip one hash digit so the address still parses but resolves to
        // content that is not present.
        let mut chars: Vec<char> = address.chars().collect();
        chars[0] = if chars[0] == '0' { '1' } else { '0' };
        let bogus: String = chars.into_iter().collect();
        assert_ne!(bogus, address, "mutation must change the address");

        let (buf, callback) = collect_callback();
        let args = LoreFileReadArgs {
            address: LoreString::from(bogus.as_str()),
            path: LoreString::default(),
            revision: LoreString::default(),
        };
        let status = lore::file::read(g.clone(), args, callback).await;
        assert_ne!(status, 0, "an unknown address must fail");
        assert!(
            buf.lock().unwrap().is_empty(),
            "no content for an unknown address"
        );
    }
}
