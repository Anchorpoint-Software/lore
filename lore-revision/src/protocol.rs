// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use crate::lore_debug;

// Bridge the connect() function to maintain the 3-argument signature.
pub async fn connect(
    remote_url: &str,
    identity: &str,
    repository: crate::lore::RepositoryId,
) -> Result<std::sync::Arc<lore_transport::Connection>, lore_transport::ProtocolError> {
    let execution = crate::lore::execution_context();
    let globals = execution.globals();
    let connection = lore_transport::connect(
        remote_url,
        identity,
        repository,
        globals.max_connections as usize,
        globals.identity_token(),
        globals.access_token(),
    )
    .await?;
    take_server_compression_mode(&connection);
    Ok(connection)
}

/// Takes the compression mode the server stated in the environment it answered the connection
/// with, unless a mode is already selected.
///
/// Applied here because connecting is what reads that environment, and because the mode lives in
/// `lore_storage`, which `lore_transport` cannot reach: it depends on the transport rather than the
/// other way around. A mode the server names that this client cannot write under is passed over,
/// the preference being advice and a mode `compress` refuses failing the first write instead.
fn take_server_compression_mode(connection: &lore_transport::Connection) {
    let Some(mode) = connection
        .environment
        .compression_mode()
        .and_then(lore_storage::writable_compression_mode)
    else {
        return;
    };
    if lore_storage::suggest_compression_mode(mode) {
        lore_debug!("Compressing payloads as the server states it prefers: {mode:?}");
    }
}
