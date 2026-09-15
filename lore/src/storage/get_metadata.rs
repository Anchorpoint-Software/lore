// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! `lore_storage_get_metadata` — fetch fragment metadata without payload bytes.
//!
//! Each item resolves to a single terminal `GET_METADATA_ITEM_COMPLETE` event carrying
//! `{id, address, fragment, error_code}`. On success `error_code == None` and `fragment`
//! carries `flags`, `size_payload`, and `size_content`. On miss `error_code ==
//! ADDRESS_NOT_FOUND` and `fragment` is the default value.
//!
//! Per item the resolution path is:
//! 1. Local probe via `ImmutableStore::query(partition, address, MatchFull)`. On exact match,
//!    emit the resolved `Fragment` and short-circuit.
//! 2. On local miss, fall through to the configured remote (if any) via
//!    `StorageSession::get_metadata`. The wire op carries no payload bytes — only Fragment.
//! 3. On remote miss or no remote configured, emit `ADDRESS_NOT_FOUND`.
//!
//! Only step 2 costs a task: the local probe runs in the calling task for every item, so a batch
//! whose items all hit locally spawns nothing.
//!
//! Short-circuits: `address.hash == Hash::default()` emits an empty `Fragment` with
//! `error_code = None` and no store work — symmetric with `lore_storage_get`.
//!
//! Successful remote fetches are not cached locally — there is no payload to cache, and
//! re-fetching metadata is cheap.

use std::sync::Arc;

use lore_base::error::InvalidArguments;
use lore_base::lore_spawn;
use lore_base::types::Address;
use lore_base::types::Fragment;
use lore_base::types::Hash;
use lore_base::types::Partition;
use lore_error_set::prelude::*;
use lore_macro::LoreArgs;
use lore_macro::ValidateText;
use lore_revision::event::EventError;
use lore_revision::event::LoreErrorCode;
use lore_revision::event::LoreEvent;
use lore_revision::interface::LoreArray;
use lore_revision::interface::LoreError;
use lore_revision::store::event::LoreStorageGetMetadataItemCompleteEventData;
use lore_storage::store_types::StoreMatch;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinSet;

use crate::call_delegation::dispatch_call;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;
use crate::storage::call::storage_call;
use crate::storage::handle::LoreStore;
use crate::storage::store::EffectiveFlags;
use crate::storage::store::SessionReuse;
use crate::storage::store::StoreInternal;

/// One `get_metadata` item — the `(partition, address)` to look up.
#[repr(C)]
#[derive(Copy, Clone, Default, Debug, PartialEq, Deserialize, Serialize, ValidateText)]
pub struct LoreStorageGetMetadataItem {
    /// Caller-chosen id echoed back in `GET_METADATA_ITEM_COMPLETE`
    pub id: u64,
    /// Partition to look up; the zero/default partition rejects with `INVALID_ARGUMENTS`
    pub partition: Partition,
    /// Content address to look up; `hash == Hash::default()` short-circuits to an empty fragment
    pub address: Address,
}

/// Arguments for `lore_storage_get_metadata`.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize, LoreArgs)]
#[handler(get_metadata_local)]
pub struct LoreStorageGetMetadataArgs {
    /// Open storage handle
    pub handle: LoreStore,
    /// Addresses to look up; each runs independently and emits its own `GET_METADATA_ITEM_COMPLETE`
    pub items: LoreArray<LoreStorageGetMetadataItem>,
}

#[error_set]
enum GetMetadataError {
    InvalidArguments,
}

impl EventError for GetMetadataError {
    fn translated(&self) -> LoreError {
        match self {
            GetMetadataError::InvalidArguments(_) => LoreError::InvalidArguments,
            GetMetadataError::Internal(_) => LoreError::Internal,
        }
    }

    fn inner(&self) -> String {
        self.to_string()
    }
}

/// Fetch fragment metadata for one or more addresses without paying the payload bytes.
pub async fn get_metadata(
    globals: LoreGlobalArgs,
    args: LoreStorageGetMetadataArgs,
    callback: LoreEventCallback,
) -> i32 {
    dispatch_call(globals, args, callback, get_metadata_local).await
}

async fn get_metadata_local(
    globals: LoreGlobalArgs,
    args: LoreStorageGetMetadataArgs,
    callback: LoreEventCallback,
) -> i32 {
    let handle = args.handle;
    let per_call = crate::storage::store::PerCallFlags::from_globals(&globals);
    storage_call(
        globals,
        callback,
        handle,
        args,
        get_metadata,
        async move |store, args| {
            let items = args.items.as_slice();
            if items.is_empty() {
                return Ok::<(), GetMetadataError>(());
            }
            let effective = store.effective_flags(per_call)?;

            let total = items.len();
            let mut remote_tasks: JoinSet<LoreErrorCode> = JoinSet::new();
            let mut codes: Vec<LoreErrorCode> = Vec::with_capacity(total);
            let mut reuse = crate::storage::store::SessionReuse::default();

            for item in items.iter().copied() {
                let session = match item_backend(&store, &item, effective, &mut reuse).await {
                    ItemBackend::Done(code) => {
                        codes.push(code);
                        continue;
                    }
                    ItemBackend::Remote(session) => session,
                };

                if total == 1 {
                    codes.push(resolve_remote(session, item).await);
                } else {
                    lore_spawn!(
                        remote_tasks,
                        async move { resolve_remote(session, item).await }
                    );
                }
            }

            codes.extend(crate::storage::drain_codes(remote_tasks).await);
            crate::storage::build_call_error(&codes, total, "get_metadata")
        },
    )
    .await
}

/// Where one item's answer comes from, once the local store has had its say.
enum ItemBackend {
    /// The item is settled — its terminal event is emitted and this is the code it carried.
    Done(LoreErrorCode),
    /// The item missed locally and this session is the only place left to ask.
    Remote(Arc<lore_transport::StorageSession>),
}

/// Route one item to the backend that can answer it. `Done` covers every outcome the local store
/// settles plus a miss with no remote to consult, all of which have emitted their terminal event
/// already; `Remote` is the sole outcome that still owes a wire round trip, and so the only one
/// worth a task.
///
/// `no_local` skips the probe: the local store is not this call's to read. It cannot coincide with
/// `no_remote` — `effective_flags` rejects a request for both — so a `want_remote` of
/// `!no_remote` resolves a session on the remote-bound path and suppresses one on the
/// local-bound path, where a local miss is the final answer.
async fn item_backend(
    store: &Arc<StoreInternal>,
    item: &LoreStorageGetMetadataItem,
    effective: EffectiveFlags,
    reuse: &mut SessionReuse,
) -> ItemBackend {
    if !effective.no_local
        && let Some(code) = resolve_local(store, item).await
    {
        return ItemBackend::Done(code);
    }

    match reuse.session_for(store, item.partition, !effective.no_remote) {
        Some(session) => ItemBackend::Remote(session),
        None => ItemBackend::Done(emit_complete(
            item,
            Fragment::default(),
            LoreErrorCode::AddressNotFound,
        )),
    }
}

/// Probe the local store for one item, emitting its terminal event and returning the code on
/// every outcome the local store settles: a rejected argument, the zero-hash short-circuit, a
/// hit, and a non-not-found error, which is surfaced rather than masked by a remote attempt.
/// `None` means the item missed and the remote may still answer it.
///
/// Any match the store made is a hit, not just a full one. This operation answers what a payload
/// *is*, and a weaker level names the same bytes under the same hash — reached under a context or
/// partition the caller did not name, which changes whose association it is and not what the
/// content decodes to. Requiring `MatchFull` here spent a round trip re-fetching a description the
/// store had already produced.
async fn resolve_local(
    store: &Arc<StoreInternal>,
    item: &LoreStorageGetMetadataItem,
) -> Option<LoreErrorCode> {
    if item.partition == Partition::default() {
        return Some(emit_complete(
            item,
            Fragment::default(),
            LoreErrorCode::InvalidArguments,
        ));
    }

    if item.address.hash == Hash::default() {
        return Some(emit_complete(
            item,
            Fragment::default(),
            LoreErrorCode::None,
        ));
    }

    match store
        .immutable
        .clone()
        .get_metadata(item.partition, item.address)
        .await
    {
        Ok(result) if result.match_made != StoreMatch::MatchNone => {
            Some(emit_complete(item, result.fragment, LoreErrorCode::None))
        }
        Ok(_) => None,
        Err(err) if err.is_address_not_found() => None,
        Err(err) => Some(emit_complete(
            item,
            Fragment::default(),
            crate::storage::store_error_to_code(&err),
        )),
    }
}

/// Resolve one item against the remote. Emits the terminal event with the wire-fetched Fragment
/// on success, or a mapped error code via the canonical `protocol_error_to_storage` →
/// `storage_error_to_code` chain on any failure.
async fn resolve_remote(
    session: Arc<lore_transport::StorageSession>,
    item: LoreStorageGetMetadataItem,
) -> LoreErrorCode {
    match session.get_metadata(&item.address).await {
        Ok(fragment) => emit_complete(&item, fragment, LoreErrorCode::None),
        Err(err) => {
            let storage_err = lore_storage::error::protocol_error_to_storage(err, item.address);
            emit_complete(
                &item,
                Fragment::default(),
                crate::storage::storage_error_to_code(&storage_err),
            )
        }
    }
}

/// Emit the item's terminal event and return the `error_code` that was sent, so callers can
/// `return emit_complete(..)` directly.
fn emit_complete(
    item: &LoreStorageGetMetadataItem,
    fragment: Fragment,
    error_code: LoreErrorCode,
) -> LoreErrorCode {
    let address = if error_code == LoreErrorCode::None {
        item.address
    } else {
        Address::default()
    };
    LoreEvent::StorageGetMetadataItemComplete(LoreStorageGetMetadataItemCompleteEventData {
        id: item.id,
        address,
        fragment,
        error_code,
    })
    .send();
    error_code
}
