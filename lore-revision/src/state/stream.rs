// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT

use lore_base::lore_spawn;
use lore_error_set::prelude::*;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::change::NodeChange;
use crate::state::StateError;

/// Where a diff walk emits the changes it finds.
///
/// Cloned into each subtree a walk spawns, so a subtree emits to the caller directly rather than
/// collecting for a parent to fold in. Carries changes alone: what a walk could not answer is
/// its own failure, reported where it ends rather than in place of a change.
pub type ChangeSender = mpsc::Sender<NodeChange>;

/// Emits one change to the caller reading them.
///
/// A closed channel is a caller that has stopped listening, which the walk learns of here: the
/// error unwinds it, and a caller that closed deliberately already has its answer and discards
/// that verdict.
pub(crate) async fn emit(changes: &ChangeSender, change: NodeChange) -> Result<(), StateError> {
    changes
        .send(change)
        .await
        .map_err(|_closed| StateError::internal("Diff receiver dropped"))
}

/// How many changes a diff may run ahead of the caller reading them.
///
/// A walk that outruns its reader is holding change records nobody has looked at, which is what
/// taking them one at a time is for; a walk held to one at a time spends its parallelism waiting.
const CHANGE_LOOKAHEAD: usize = 1000;

/// The changes a diff finds, as it finds them, and what the walk reports once it ends.
///
/// Owns the walk as well as its output. Dropping this closes the channel, so the walk unwinds at
/// its next emit rather than running on for a caller that has gone — which is how a caller
/// probing for one kind of change stops the walk once it has found one.
///
/// Four ways to read it, and which one a caller wants is what it means to have this:
///
/// - [`collect`](Self::collect) for a caller that transforms the change set as a whole
/// - [`next`](Self::next) then [`finish`](Self::finish) for one reading each change once
/// - [`any`](Self::any) for one asking whether a change of some kind is there at all
/// - dropping it for one that has seen enough
///
/// [`finish`](Self::finish) is not ceremony: a walk that marks as it goes leaves those marks as
/// its real answer, and only waiting for it tells a caller they are complete.
#[must_use = "dropping a change stream stops the walk it owns"]
pub struct ChangeStream<Summary> {
    changes: mpsc::Receiver<NodeChange>,
    /// The walk producing the changes, or `None` where there is nothing to walk.
    walk: Option<JoinHandle<Result<Summary, StateError>>>,
}

impl<Summary: Default + Send + 'static> ChangeStream<Summary> {
    /// Spawns `walk` behind a channel, and answers with the changes it emits.
    ///
    /// `walk` is handed the sender to emit through, so how it walks and what it reports stay its
    /// own; the depth it may run ahead by is settled here.
    pub fn spawn<Walk, Walking>(walk: Walk) -> Self
    where
        Walk: FnOnce(ChangeSender) -> Walking,
        Walking: Future<Output = Result<Summary, StateError>> + Send + 'static,
    {
        let (sender, changes) = mpsc::channel(CHANGE_LOOKAHEAD);
        ChangeStream {
            changes,
            walk: Some(lore_spawn!(walk(sender))),
        }
    }

    /// A walk with nothing to report, for a path the filter leaves out entirely.
    pub fn nothing() -> Self {
        let (sender, changes) = mpsc::channel(1);
        drop(sender);
        ChangeStream {
            changes,
            walk: None,
        }
    }

    /// The next change, or `None` once the walk has no more.
    ///
    /// A walk that failed reports it from [`finish`](Self::finish), so `None` is the end of the
    /// changes and not yet the verdict on them.
    pub async fn next(&mut self) -> Option<NodeChange> {
        self.changes.recv().await
    }

    /// Every change the walk finds.
    ///
    /// Drains as the walk produces, so the lookahead never holds a walk against a caller that is
    /// collecting. A caller that wants what the walk reported reads the changes with
    /// [`next`](Self::next) and then asks [`finish`](Self::finish).
    pub async fn collect(self) -> Result<Vec<NodeChange>, StateError> {
        let ChangeStream { mut changes, walk } = self;
        let mut collected = Vec::new();
        while let Some(change) = changes.recv().await {
            collected.push(change);
        }
        joined(walk).await?;
        Ok(collected)
    }

    /// Whether the walk finds a change `wanted` accepts, answered at the first one that does.
    ///
    /// Stops the walk there rather than reading the rest: one change is the whole of the answer,
    /// and a walk still looking for a second is work nobody asked for. The walk ends at its next
    /// emit, so what it would have reported goes unread — which is why this is for a walk whose
    /// only output is the changes it was cut short of finding.
    pub async fn any(self, wanted: impl Fn(&NodeChange) -> bool) -> Result<bool, StateError> {
        let ChangeStream { mut changes, walk } = self;
        while let Some(change) = changes.recv().await {
            if wanted(&change) {
                return Ok(true);
            }
        }
        joined(walk).await?;
        Ok(false)
    }

    /// What the walk reported, for a caller that has read the changes it came for.
    ///
    /// Lets the walk finish rather than cutting it short, so a marking walk's flags are complete
    /// when this answers. A caller that wants it stopped drops the stream instead.
    pub async fn finish(self) -> Result<Summary, StateError> {
        let ChangeStream { mut changes, walk } = self;
        while changes.recv().await.is_some() {}
        joined(walk).await
    }
}

/// What a joined walk reported, and nothing where there was no walk.
async fn joined<Summary: Default>(
    walk: Option<JoinHandle<Result<Summary, StateError>>>,
) -> Result<Summary, StateError> {
    match walk {
        Some(walk) => walk
            .await
            .internal("Diff task failed")
            .map_err(StateError::from)?,
        None => Ok(Summary::default()),
    }
}
