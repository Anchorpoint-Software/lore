// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-FileCopyrightText: 2026 Anchorpoint Software GmbH
// SPDX-License-Identifier: MIT
use bytes::Bytes;
use lore_base::runtime::runtime;
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::mpsc::WeakUnboundedSender;
use tokio::sync::mpsc::unbounded_channel;
use tokio_util::sync::CancellationToken;

use crate::event::EventError;
use crate::event::LoreCompleteEventData;
use crate::event::LoreEndEventData;
use crate::event::LoreErrorEventData;
use crate::event::LoreEvent;
use crate::event::LoreLogEventData;
use crate::interface::LoreEventCallback;
use crate::logging::LoreLogLevel;
use crate::util;

/// Item sent through the mpsc event channel. Each event may carry an
/// optional `Bytes` keepalive that pins a buffer referenced by the
/// event's payload for the duration of the callback invocation.
///
/// The events that use the keepalive today are
/// `LoreEvent::StorageGetData` and `LoreEvent::FileRead`, whose `LoreBytes`
/// view points into the
/// carried `Bytes`. The forwarder holds the `Bytes` clone while the
/// callback runs, then drops it. Since `Bytes` is itself refcounted,
/// the caller's task may drop its own clone as soon as `send_with_bytes`
/// returns — the buffer stays alive until every registered keepalive
/// has been consumed.
type DispatchedEvent = (LoreEvent, Option<Bytes>);

pub struct EventDispatcher {
    pub correlation_id: String,
    pub completed: CancellationToken,
    pub weak_sender: Option<WeakUnboundedSender<DispatchedEvent>>,
    pub strong_sender: Mutex<Option<UnboundedSender<DispatchedEvent>>>,
}

impl Default for EventDispatcher {
    fn default() -> Self {
        Self {
            correlation_id: String::default(),
            completed: CancellationToken::new(),
            weak_sender: None,
            strong_sender: Mutex::new(None),
        }
    }
}

impl EventDispatcher {
    #[allow(clippy::disallowed_methods)]
    pub fn new(callback: LoreEventCallback) -> Self {
        let completed = CancellationToken::new();
        let (sender, mut receiver) = unbounded_channel();
        let weak_sender = sender.downgrade();
        if let Some(callback) = callback {
            let completed = completed.clone();

            // Spawn a forwarder task which will exit once all dispatchers
            // have terminated and mpsc channel has no producers. Each
            // item carries an optional `Bytes` keepalive; the forwarder
            // drops it AFTER the callback returns, so any `LoreBytes`
            // view in the event points at a live buffer for the full
            // callback invocation.
            runtime().spawn(async move {
                while let Some((event, _keepalive)) = receiver.recv().await {
                    callback(&event);
                    // `_keepalive` drops here — the referenced buffer
                    // is released after the callback has finished.
                }
                callback(&LoreEvent::End(LoreEndEventData::default()));
                completed.cancel();
            });
        } else {
            completed.cancel();
        };

        Self {
            correlation_id: String::default(),
            completed,
            weak_sender: Some(weak_sender),
            strong_sender: Mutex::new(Some(sender)),
        }
    }

    pub fn no_dispatch() -> Self {
        Self {
            correlation_id: String::default(),
            completed: CancellationToken::new(),
            weak_sender: None,
            strong_sender: Mutex::new(None),
        }
    }

    pub fn sender(&self) -> Option<UnboundedSender<DispatchedEvent>> {
        self.weak_sender
            .as_ref()
            .and_then(|sender| sender.upgrade())
    }

    pub fn send(&self, event: LoreEvent) {
        self.send_inner(event, None);
    }

    /// Emit an event whose payload references a caller-owned buffer.
    /// The `Bytes` clone travels with the event through the channel and
    /// is dropped only after the forwarder has returned from the
    /// user callback — keeping the bytes valid for the duration of the
    /// callback invocation without requiring the caller's task to
    /// outlive the dispatch.
    pub fn send_with_bytes(&self, event: LoreEvent, bytes: Bytes) {
        self.send_inner(event, Some(bytes));
    }

    fn send_inner(&self, event: LoreEvent, keepalive: Option<Bytes>) {
        if let Some(sender) = self.sender()
            && let Err(_err) = sender.send((event, keepalive))
        {
            /*
            generate_log(
                self.correlation_id.as_str(),
                LoreLogLevel::Trace,
                format!("Failed to send event: {err}"),
            );
            */
        }
    }

    pub fn send_error(&self, error: impl EventError) {
        crate::lore_error!("{}", error.inner());
        self.send(LoreEvent::Error(LoreErrorEventData::from_inner_error(
            &error,
        )));
    }

    pub async fn complete(&self, status: i32) {
        self.send(LoreEvent::Complete(LoreCompleteEventData { status }));

        // Drop this strong reference, let the dispatcher task exit out and signal the end event
        // if this is the only strong reference to the event channel
        drop(self.strong_sender.lock().await.take());

        // If there are other strong references remaining it means the end event will come
        // whenever that completes (such as an ongoing notification subscription)
        if self
            .weak_sender
            .as_ref()
            .map(|sender| sender.strong_count())
            .unwrap_or_default()
            == 0
        {
            self.completed.cancelled().await;
        }
    }

    pub fn make_log(level: LoreLogLevel, message: String) -> LoreLogEventData {
        LoreLogEventData {
            level,
            category: 0,
            timestamp: util::time::timestamp(),
            location: Default::default(),
            message: message.into(),
        }
    }
}
