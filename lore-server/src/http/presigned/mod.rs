// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
pub mod repository;

use std::sync::Arc;

use axum::Router;

use crate::http::server::ServerState;

pub fn create_router(state: Arc<ServerState>) -> Router {
    Router::new().nest("/{repository_id}", repository::create_router(state))
}
