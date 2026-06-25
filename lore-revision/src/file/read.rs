// SPDX-FileCopyrightText: 2026 Anchorpoint Software GmbH
// SPDX-License-Identifier: MIT
use std::sync::Arc;

use bytes::Bytes;
use lore_error_set::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::errors::*;
use crate::event;
use crate::event::EventError;
use crate::event::LoreBytes;
use crate::immutable;
use crate::interface::LoreError;
use crate::lore::Address;
use crate::lore::execution_context;
use crate::repository::RepositoryContext;
use crate::revision;
use crate::state;
use crate::util::path::RelativePath;

/// Data for the event carrying file content read into memory. `bytes` is a
/// borrowed view valid only for the callback invocation; the read emits one
/// event per fragment, each with a running `offset`.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoreFileReadEventData {
    /// Address of the content.
    pub address: Address,
    /// Byte offset of this payload within the file content.
    pub offset: u64,
    /// Total content size in bytes.
    pub size_content: u64,
    /// Payload bytes for this part of the content.
    pub bytes: LoreBytes,
}

#[error_set]
pub enum ReadError {
    InvalidArguments,
    InvalidPath,
    InvalidAddress,
    RevisionNotFound,
    FileNotFound,
    WriteRequired,
    AddressNotFound,
    Disconnected,
    InvalidNodeHierarchy,
    LinkNotFound,
    Maintenance,
    NodeNotFound,
    NoRemote,
    NotAuthenticated,
    NotAuthorized,
    NotConnected,
    NotFound,
    NotSupported,
    Oversized,
    PayloadNotFound,
    SlowDown,
    AlreadyLinked,
    BranchAdvanced,
    BranchAlreadyExists,
    BranchNotFound,
    Conflict,
    DeleteCurrent,
    DeleteDefault,
    DeleteProtected,
    Divergent,
    IdenticalMetadata,
    LayerNotFound,
    LinkPathNotFound,
    LocalModifications,
    LockNotFound,
    LockNotOwned,
    MaxHistorySearchDepth,
    NotALayer,
    NotALink,
    NothingStaged,
    RepositoryAlreadyExists,
    RepositoryNotFound,
    SharedStoreNotFound,
    TokenNotFound,
    MissingIdentity,
}

impl EventError for ReadError {
    fn translated(&self) -> LoreError {
        match self {
            ReadError::InvalidArguments(_)
            | ReadError::InvalidPath(_)
            | ReadError::InvalidAddress(_) => LoreError::InvalidArguments,
            ReadError::RevisionNotFound(_) | ReadError::NotFound(_) => LoreError::NotFound,
            ReadError::FileNotFound(_) => LoreError::FileNotFound,
            _ => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

#[derive(Clone, Debug)]
pub struct ReadFileOptions {
    pub revision: Option<String>,
}

pub async fn read_file(
    repository: Arc<RepositoryContext>,
    path: String,
    options: ReadFileOptions,
) -> Result<(), ReadError> {
    let relative_path = RelativePath::new_from_user_path(repository.require_path()?, path.as_str())
        .forward::<ReadError>("resolving user path")?;

    let signature = if let Some(revision) = options.revision {
        revision::resolve(
            repository.clone(),
            revision.as_str(),
            execution_context().globals().search_limit(),
            execution_context().globals().search_location(),
        )
        .await
        .map_err(|_err| {
            ReadError::from(RevisionNotFound {
                revision: revision.clone(),
            })
        })?
    } else {
        let (current_revision, _current_branch) = crate::instance::load_current_anchor(&repository)
            .await
            .forward::<ReadError>("Failed to deserialize current revision anchor")?;
        crate::instance::load_staged_revision(&repository)
            .await
            .ok()
            .flatten()
            .unwrap_or(current_revision)
    };

    let state = state::State::deserialize(repository.clone(), signature)
        .await
        .forward::<ReadError>("Failed to deserialize state")?;

    let node_link = state
        .find_node_link(repository.clone(), relative_path.as_str())
        .await
        .map_err(|_err| {
            ReadError::from(FileNotFound {
                resource: relative_path.to_string(),
            })
        })?;
    if !node_link.is_valid() {
        return Err(FileNotFound {
            resource: relative_path.to_string(),
        }
        .into());
    }

    let node = state
        .node(repository.clone(), node_link.node)
        .await
        .map_err(|_err| {
            ReadError::from(FileNotFound {
                resource: relative_path.to_string(),
            })
        })?;

    if !node.is_file() {
        return Err(FileNotFound {
            resource: relative_path.to_string(),
        }
        .into());
    }

    if node.size == 0 {
        emit(node.address, 0, 0, Bytes::new());
        return Ok(());
    }

    read_content(repository, node.address).await
}

pub async fn read_address(
    repository: Arc<RepositoryContext>,
    address: Address,
) -> Result<(), ReadError> {
    read_content(repository, address).await
}

async fn read_content(
    repository: Arc<RepositoryContext>,
    address: Address,
) -> Result<(), ReadError> {
    let options = immutable::read_options_from_repository(&repository);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Bytes>(256);
    let size_content = immutable::read_stream(repository.clone(), address, options, tx)
        .await
        .forward::<ReadError>("reading immutable data")?;

    let mut offset: u64 = 0;
    while let Some(chunk) = rx.recv().await {
        let len = chunk.len() as u64;
        emit(address, offset, size_content, chunk);
        offset += len;
    }

    // The defragment pipeline reports a mid-stream failure by dropping the
    // sender, which ends the loop early; a short read must surface as an error
    // rather than a truncated success.
    if offset != size_content {
        return Err(ReadError::internal(format!(
            "streamed {offset} bytes, expected {size_content}"
        )));
    }

    Ok(())
}

/// Emit a `FileRead` event whose `LoreBytes` view points into `bytes`, attaching
/// `bytes` as the callback-lifetime keepalive so the view stays valid for the
/// full callback invocation.
fn emit(address: Address, offset: u64, size_content: u64, bytes: Bytes) {
    let view = LoreBytes {
        ptr: bytes.as_ptr().cast(),
        len: bytes.len(),
    };
    let event = event::LoreEvent::FileRead(LoreFileReadEventData {
        address,
        offset,
        size_content,
        bytes: view,
    });
    execution_context().dispatcher.send_with_bytes(event, bytes);
}
