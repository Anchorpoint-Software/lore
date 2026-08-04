// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Completion-based asynchronous file I/O.
//!
//! This crate provides positional, owned-buffer file operations whose
//! futures are runtime-independent: they suspend on wakers only and never
//! require a specific async runtime to be driving them. Operations take
//! ownership of their buffer and hand it back on completion, the contract
//! required by completion-based backends (`io_uring`, overlapped I/O)
//! where the kernel holds a pointer into the buffer while an operation is
//! in flight.
//!
//! Backends implement one driver trait surface:
//!
//! - `psync` — positional syscalls executed on a dedicated bounded syscall
//!   pool (`min(2 × cores, 16)` threads, idle-reaped). The portable
//!   baseline backend and the semantic reference for all others.
//!
//! `io_uring` (Linux) and overlapped I/O (Windows) backends are planned
//! to slot in behind the same API. See the enhancement proposal
//! `2026-07-24-tokio-runtime-split-and-async-io` for the design.

mod buffer;
mod driver;
mod file;
#[cfg(target_family = "windows")]
mod overlapped;
mod pool;
mod psync;

pub use buffer::StableBuf;
pub use pool::PoolStats;

/// A snapshot of the process-wide syscall pool, which every driver and every backend shares.
///
/// Exposed so the thread budget can be checked against a real workload rather than assumed: the
/// high-water marks say whether the cap is ever reached, and a deep queue alongside idle threads
/// says the cap is not what is limiting the work.
pub fn pool_stats() -> PoolStats {
    pool::SyscallPool::global().stats()
}
pub use driver::BackendKind;
pub use driver::IoDriver;
pub use driver::WHOLE_FILE_LIMIT;
pub use file::IoFile;
pub use file::OpenOptions;
