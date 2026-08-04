# File I/O engine

The `lore-io` crate is Lore's file I/O engine: positional, owned-buffer file operations whose futures suspend on wakers alone and are therefore independent of the runtime driving them. It is the intended replacement for every `std::fs` and `tokio::fs` call inside the library. The crate is a workspace member with no dependents yet — the migration described below hasn't started — so today it's exercised only by its own conformance suite and benchmark example.

Measurements quoted here are summaries. `lore-io/BENCHMARKS.md` has the results, the protocol, and the experiments behind them.

## Driver and backend selection

`IoDriver` is the entry point. It holds an `Arc<DriverInner>`, an enum with one variant per backend, and every operation dispatches through a match on that enum rather than through a trait object. Cloning an `IoDriver` bumps a reference count, and clones share the backend.

| Backend | Selector | Implementation |
| --- | --- | --- |
| `psync` | `BackendKind::Psync` | Positional syscalls on the shared syscall pool. `pread`/`pwrite` through `FileExt::read_at`/`write_at` on Unix; `ReadFile`/`WriteFile` with the crate's own `OVERLAPPED` on Windows. |
| `auto` | `BackendKind::Auto` | Probes for the best available backend. Resolves to `psync`, the only backend compiled today. |

`IoDriver::from_env` reads `LORE_IO_BACKEND`, accepting `auto`, `psync`, and the empty string; any other value is an `InvalidInput` error naming the accepted set. `IoDriver::global()` initializes one driver per process from the environment on first use, and on an unrecognized value reports it and falls back to the probed backend rather than failing — the variable is for diagnosis and rollback, and a typo in it should not take down a host application on its first file read. `backend_name()` returns the resolved backend for logs and benchmark labels.

## Windows handles are overlapped

Handles the driver opens carry `FILE_FLAG_OVERLAPPED`, and the data path issues `ReadFile`/`WriteFile` through `overlapped.rs` rather than `std`'s `seek_read`/`seek_write`. Both halves are load-bearing.

A handle opened *without* the flag is a synchronous file object, and the I/O manager serializes every operation on it — which contradicts the property this API is built on, a shared handle carrying concurrent operations at disjoint offsets. Measured, that serialization costs 2× warm and 3× cold. `pread` on a shared descriptor takes no equivalent lock, so nothing on Unix pays it.

`std`'s positional calls cannot be used on such a handle. They put the offset in an `OVERLAPPED`, pass no event, and report `ERROR_IO_PENDING` as an ordinary error — but that status means the kernel has accepted the request and still owns the buffer, so a caller treating it as a failure frees memory the kernel is about to write into. Each operation here therefore owns its completion: its own `OVERLAPPED`, a per-thread manual-reset event (a null event signals the *file handle*, which two concurrent operations cannot share), and a `GetOverlappedResult` wait that a pending operation cannot return past. A failed wait cancels and waits again rather than returning while the kernel can still touch the buffer.

The wait blocks, which is what the syscall pool exists for, and it is the same plumbing the planned completion-port backend needs. It is also dead code so far: no measured operation has reported `ERROR_IO_PENDING`, on any host, warm or cold. It exists because the status is reachable — SMB shares, sparse and compressed files, filter drivers that punt — and because the failure mode without it is the kernel writing into freed memory. A test that forces it is owed.

Two consequences beyond throughput. Overlapped handles maintain no file cursor, so `positional_reads_have_no_cursor` in the conformance suite pins a property of the handle rather than of the API's discipline. And the operations work on synchronous handles too, which they must: the whole-file composites open their own.

One ceiling this does not lift. Concurrent reads against a *single* file plateau on Windows around 91k ops/s however many handles or threads are used, while the same reads over distinct files scale to 477k. That is a cache-manager property no backend can route around, and it bounds any single-file workload on the platform. The storage layer's are not among them: addresses are hash-distributed over 256 pack-file groups whose files roll at 3 GiB, so a server holds thousands and concurrent reads scatter across them rather than concentrating on one. The ceiling is specific to Windows — on macOS, spreading the same reads over one file per thread raises throughput rather than lifting a plateau, so nothing is serialized on the file.

## Buffer ownership

Completion-based backends hand the kernel a pointer into a buffer and keep it there while the operation is in flight, so the API cannot borrow caller memory. The two directions solve that differently, because the memory has different origins.

**Writes take the caller's buffer by value** and return it on completion. `StableBuf` marks the types whose backing memory does not move for the value's lifetime: `Bytes` and `Vec<u8>` implement it. The data is the caller's, so there is nothing to allocate.

**Reads allocate their own buffer, inside the submitted job, and return `Bytes`.** No caller buffer is involved, which satisfies the completion contract directly: whoever owns the memory holds it until the kernel is done, and here that owner is the job. Two consequences follow:

- The allocation happens on the thread that receives the kernel's copy. The process allocator is rpmalloc (`lore-base/src/allocator`), which caches spans per thread, so buffer reuse is thread-local without this crate managing it.
- `Bytes` is what the storage layer traffics in, so nothing is copied at the boundary. `BytesMut::freeze` makes the conversion free.

Read buffers are allocated uninitialised through `buffer::uninit_buffer` and truncated to the byte count before becoming `Bytes`, so no uninitialised byte is ever observable. Zeroing first would write every byte twice, about a tenth of the benchmark's CPU time.

There is no buffer pool. An earlier design had one, size-classed and retention-bounded; measurement showed it neither helped nor was needed, since the allocator already caches per thread with the right locality.

The one capability this shape does not offer is reading into memory the caller already owns — filling a window in place. Nothing needs it today; a chunker port that wants it would need the API back.

## File handle operations

`IoFile` pairs an `Arc<std::fs::File>` with the driver that opened it. Cloning shares the handle. There is no file cursor: every operation is positional, and concurrent operations on one handle at disjoint offsets are safe and unordered.

| Operation | Behavior |
| --- | --- |
| `read_at(max_len, offset)` | Reads up to `max_len` bytes, returning what arrived. Fewer than `max_len` means the file ended; empty means the offset was already at or past the end. |
| `read_exact_at(len, offset)` | Loops until `len` bytes are read. `UnexpectedEof` if the file ends first. |
| `write_at(buffer, offset)` | Writes the buffer contents. Returns the buffer and the byte count. |
| `write_all_at(buffer, offset)` | Loops until the whole buffer is written. `WriteZero` if the file stops accepting bytes. |
| `sync_data()` / `sync_all()` | `fdatasync` / `fsync`. |
| `metadata()`, `set_len(len)` | Handle-scoped `stat`, and resize. Extending leaves a hole that reads as zeros. |

Reads take their length as an argument and writes take theirs from the buffer, because a read has nothing to infer a length from until the caller says how much it wants and a write already holds the bytes.

The looping forms complete in a single backend dispatch: the loop runs inside the submitted job, which already owns the buffer and the thread, so a short read costs another syscall rather than another round trip through the pool.

There is no preallocating operation, a deliberate departure from the operation set the proposal lists. `posix_fallocate` reserves blocks only on some Linux filesystems — on ZFS it reserves nothing, indistinguishable from `set_len` — so an operation named for reservation would guarantee different things on different mounts and no test could pin it portably. Nothing in the workspace asks for reservation: the one site that sizes a file up front, the defragment output in `lore-storage/src/defragment.rs`, does so precisely to get a hole that reads as zeros. A caller that needs blocks committed should get an operation named for that guarantee.

Path-scoped operations live on the driver: `open`, `metadata`, `rename`, `remove_file`, `create_dir_all`. `OpenOptions` mirrors the `std::fs` builder for `read`, `write`, `create`, `create_new`, and `truncate`.

Every operation dispatches through the backend, the metadata ones included. A completion backend will keep those on the syscall pool regardless — a ring-submitted `statx` is punted to a kernel worker making the same blocking call — but routing them anyway lets a backend override any operation, makes a driver instance self-contained rather than partly bypassed, and leaves the syscall pool reachable only from the backends.

## Whole-file composite operations

Two driver operations complete a whole file in a single backend dispatch, matching the atomic whole-file patterns the storage layer already uses and keeping small-file scans at one dispatch per file instead of separate open, stat, and read round trips.

- `read_file_bytes(path)` — open, stat, read to the stat length, close. Returns `Bytes` without copying. A file that shrinks mid-read fails with `UnexpectedEof`.
- `write_file_bytes(path, data, durable)` — create or truncate, write, `fdatasync` when `durable` is set, stat, close. Returns the resulting `Metadata`.

Both are bounded by `WHOLE_FILE_LIMIT`, 8 MiB, and fail with `InvalidInput` above it. Each holds a pool thread and the whole file resident for its duration, so a large file would occupy one of at most `min(2 × cores, 16)` threads for the transfer; that caller wants `open` with `read_exact_at` or `write_all_at`. The write's check runs before the open, so a rejected call cannot have truncated an existing file.

## Syscall pool

`SyscallPool` is a bounded pool of threads dedicated to blocking syscalls, independent of any tokio pool. One process-wide instance backs every operation on the `psync` backend.

| Property | Value |
| --- | --- |
| Maximum threads | `min(2 × cores, 16)`, from `available_parallelism` (2 when unavailable) |
| Spawn policy | On demand: a submission spawns a thread only when no thread is idle and the cap isn't reached |
| Idle policy | A thread exits after 10 seconds without work |
| Thread names | `lore-io-<n>` |
| Queue | Unbounded `VecDeque`, drained FIFO |

Pooled rather than inline execution is deliberate: a syscall against cold media or a network filesystem blocks for milliseconds or indefinitely, and running it inline on an async worker would let a handful of such operations stall the runtime. The cap is an eighth of the tokio blocking pool's 128-thread ceiling, and the pool is dedicated, so file I/O no longer competes with unrelated blocking work for threads.

`submit` returns a `SyscallTask<T>`, a future over a `oneshot` receiver. It resolves through an ordinary waker, so it runs under any executor — the multi-thread runtime in production, and the single-threaded runtimes `#[tokio::test]` creates. Work is wrapped in `catch_unwind`: a panic on a pool thread resumes on the awaiting task rather than killing the worker.

`pool_stats()` returns a `PoolStats` snapshot — queued, executing, live threads, and the thread and queue high-water marks, alongside the cap. It is taken under the pool's own lock, so the counts are consistent rather than independently sampled, and the marks are maintained inside the critical section the submission path already enters. The high-water figures are what makes the cap checkable against a real workload: a queue running deep while threads sit idle says the cap is not what limits the work.

The pool size is fixed at first use. `LORE_IO_POOL_THREADS` overrides the formula, rejecting zero and anything above 128 — the ceiling of the pool this engine replaces, so the knob cannot size the engine above it — and reporting an unusable value before falling back to the formula rather than failing a host application's first file read. The knob is for measurement and rollback; sizing for a deployment belongs in the `ThreadCounts` apportionment in `lore-base/src/runtime.rs`, which the engine joins when the blocking pool shrinks to its residual.

Three findings shape the formula, all detailed in `lore-io/BENCHMARKS.md`:

- **No single cap wins.** Warm work prefers fewer threads and cold work more, and the two can pull hard in opposite directions on one machine.
- **The ceiling costs single digits.** 16 threads measure within a few percent of 32 on every phase, and the threads are the return, since this pool shares a process-wide budget.
- **What costs sharply is falling below the workload's concurrency** — a ratio of pool size to in-flight requests, not to core count. Cold reads measured 0.54× at 8 threads and 0.65× at 4 against workloads offering 64 and 128. The formula reaches those sizes on machines of four cores and fewer, so a floor is the change the sweeps most support; it is not in the formula today.

The core count is every core, including ones that compute slowly. A thread parked in `pread` waits on a device rather than occupying a core, so a slow core still carries a request. Cached reads *are* core-bound and peak near the fast-core count on Apple silicon, but sizing this pool to that peak was measured and cost a fifth of the device on cold reads; that finding belongs to the `ThreadCounts` worker sizing, which is where core speed governs.

## Runtime independence

The library depends on no async runtime at all — tokio is a dev-dependency, for the tests' own runtime. Nothing calls `Handle::current()`, spawns a task, or creates a timer. Operations are submitted to the pool from whatever thread polls the future, and completion arrives through a waker.

Submitted work and the slot its result is published into share one allocation: `Task<T, F>` in `pool.rs`, held by the queue as a `Job` to run and by the future as a `Completion` to read. That replaced a boxed closure plus a channel, halving allocations per operation — measured as throughput-neutral, so it stands on allocation count rather than speed.

`operations_complete_under_a_foreign_executor` in the conformance suite drives reads, writes, whole-file operations and a cancellation under `futures::executor::block_on` with no tokio runtime present. Two consequences matter for the migration: the engine behaves identically on either side of the core/net runtime split, and a completion backend can slot in behind the same API by delivering wakeups from a reaper thread instead of from a pool thread, with no change to callers.

## Replacing `std::fs` and `tokio::fs`

Today a file operation in Lore is either a `tokio::fs` call or a `lore_spawn_blocking!` closure. Both execute the syscall on the tokio blocking pool, sized `min(2 × (cores + 1), 128)`, which means two cross-thread handoffs per operation and a parallelism ceiling set by a thread-count formula — 10 threads on a four-core laptop, where a clone materializing tens of thousands of fragments needs far more requests in flight to keep the device busy.

The proposal's survey found 34 distinct file I/O sites across `lore-storage`, `lore-revision`, and `lore-base`, none of which leak file types across a public API boundary. The migration is therefore entirely internal, and lands per subsystem — pack store, local stores, defragment, fragment engine, revision file operations — each slice green against the full test suite before the next.

Most sites are mechanical: the pack store already uses positional I/O and maps one-to-one onto `read_at` and `write_at`. Three need a structural decision:

- **Defragment data path** — the mutex-plus-seek file sink becomes concurrent `write_at` to disjoint offsets, and the parallel memory-mapped read and write variants are deleted rather than ported, removing a dual-path sink and the page-fault stalls memory mapping hides from the scheduler.
- **Bucket deserialization** — three position-dependent sequential reads become one positional read of the bucket followed by in-memory parsing.
- **Whole-file read-then-hash** — a whole-file mapping feeding a single hash call becomes a chunked read loop feeding an incremental hasher, double-buffered so the next read is in flight while the current chunk hashes. `lore-storage/src/chunker.rs` already streams fixed windows through blocking positional reads, so moving it onto `IoFile::read_at` is a substitution rather than a redesign.

File locking changes mechanism rather than structure: blocking `flock` with thread-sleep retries becomes `LOCK_NB` with async retry. Directory enumeration stays as inline syscalls: `io_uring` has no `getdents` operation, and page-cached directory walks are microsecond-scale.

Two things constrain how far the replacement goes. Operations with no asynchronous form — OS keyring access, AWS SDK initialization, service IPC pipe reads — stay on a residual blocking pool of about four threads, core-count-independent because nothing that scales with load runs there. And `std::fs` remains correct in tests, build scripts, and CLI-process code outside the library thread model; the target is the library's own I/O paths.

Progress is observable as a shrinking match count. These are raw grep line counts over `lore-*/src` and `lore/src`, including imports and test modules, so they overstate distinct call sites and serve as a trend line:

| Crate | `tokio::fs` | `std::fs::` |
| --- | --- | --- |
| `lore-revision` | 114 | 41 |
| `lore-storage` | 11 | 99 |
| `lore-server` | 2 | 30 |
| `lore` | 3 | — |
| `lore-base` | 1 | 8 |

When the migration completes, clippy `disallowed-methods` fences hold the line against direct `std::fs` and `tokio::fs` calls in library code — the same mechanism that keeps direct `tokio::spawn` out.

## Planned backends

The `psync` backend is the permanent engine on macOS, which offers no completion-based file I/O, and the fallback wherever completion-based I/O is unavailable — a common case on Linux, since Docker's default `seccomp` profile blocks `io_uring` syscalls and kernels older than 5.6 lack it. That permanence is measured: on macOS `psync` reaches the device's cold read ceiling, matching plain threads issuing the same `pread` at equal concurrency, and matches or beats `spawn_blocking` on every warm phase.

Two completion backends are planned behind the same API: `io_uring` on Linux 5.6+, and overlapped I/O on a completion port on Windows. Both split the data plane (read, write, flush) onto the kernel interface while metadata and composite operations stay on the syscall pool, because a ring-submitted `openat` or `statx` is punted to a kernel worker making the same blocking call. Completions arrive on a single reaper thread that parks in the kernel wait call, drains in batches, and invokes each operation's waker.

The Windows one is the shorter of the two: its handles, per-operation `OVERLAPPED` and kernel-owned buffers are already what `psync` uses, so binding the handle to a port and moving the wait from `GetOverlappedResult` to the reaper is what remains. What it would buy is unestablished, and worth measuring before it is built. Every Windows measurement so far completes inline, so the wait a completion port replaces is not currently on the path, and the engine already matches plain blocking threads at equal concurrency on cold reads. The gain would have to come from fewer threads held across an operation, or from batching completions; neither is measured, and the per-file ceiling above bounds whatever it is on single-file workloads.

Neither backend exists yet, and neither does the per-repository `io.backend` override the proposal describes. `BackendKind` and the `LORE_IO_BACKEND` parser both grow a variant when they land.

## Source pointers

- `lore-io/src/driver.rs::IoDriver`, `::BackendKind` — backend selection and operation dispatch.
- `lore-io/src/psync.rs::PsyncDriver` — the positional-syscall backend and the platform `read_at`/`write_at` shims.
- `lore-io/src/overlapped.rs` — the Windows positional operations, the per-thread completion event, and why `std`'s shims are unusable against an overlapped handle.
- `lore-io/src/pool.rs::SyscallPool`, `::SyscallTask`, `::default_max_threads` — the bounded pool and its runtime-independent completion future.
- `lore-io/src/buffer.rs::StableBuf`, `::uninit_buffer` — the write-side buffer contract, and the uninitialised allocation reads fill.
- `lore-io/src/file.rs::IoFile`, `::OpenOptions` — the positional handle operations.
- `lore-io/tests/conformance.rs` — the semantic reference every backend must satisfy; new backends join the `drivers` list.
- `lore-io/examples/bench.rs` — the comparison against `spawn_blocking` and against `tokio::fs`, one engine per process. Results, protocol and experiments are in `lore-io/BENCHMARKS.md`, alongside `examples/pool-sweep.sh` for the cap sweep and `examples/build-ab.sh` for an A/B of two builds.
- `lore-base/src/runtime.rs` — the core and net runtime accessors and the thread budget the engine is sized against.

## See also

- The enhancement proposal `docs/proposals/2026-07-24-tokio-runtime-split-and-async-io.md` records the decision, the thread-budget arithmetic, the rejected alternatives, and the migration slicing.
- [System design](../../explanation/system-design.md) — where the storage layer's fragment sizes, which the buffer classes mirror, come from.
