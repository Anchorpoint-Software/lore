<!--
SPDX-FileCopyrightText: 2026 Epic Games, Inc.
SPDX-License-Identifier: MIT
-->

---
status: accepted
date: 2026-09-14
---

# ADR-00019: Portable aarch64 Linux build, with Neoverse V1 tuning opt-in

## Context and Problem Statement

`.cargo/config.toml` set `-C target-cpu=neoverse-512tvb` unconditionally for the
`aarch64-unknown-linux-gnu` target. That flag bundles two things: ISA features (SVE, dot-product,
crypto, LSE atomics) and a scheduling/tuning model (instruction latencies, vectorizer cost model).
The result is a binary that faults with `SIGILL` on any aarch64 Linux host that isn't Graviton3+ or
a Neoverse V1/V2 server part — Apple Silicon under Linux VMs, Ampere Altra, Raspberry Pi, and so on.

We want both: a portable `aarch64-unknown-linux-gnu` build that runs anywhere, and a tuned build for
Graviton3+, from one source tree, without an environment variable deciding which one comes out.

## Decision Drivers

- Must not rely on `RUSTFLAGS` or other env vars — those are invisible in build logs and easy to
  leave set by accident.
- Must be a build-time choice, not runtime dispatch: the tuning half of `target-cpu` (scheduling
  model) isn't expressible as `#[target_feature]` multiversioning, only the ISA-feature half is.
- The C code compiled by `lore-base/build.rs` (rpmalloc) must stay in lockstep with whatever the
  Rust side chose, without inferring that choice from an incidental proxy.

## Considered Options

- Runtime CPU-feature dispatch (`is_aarch64_feature_detected!` + `#[target_feature]`)
- Per-target-triple config files, one per variant, always explicitly selected
- Portable baseline in `.cargo/config.toml` + a small delta file merged in via `--config`

## Decision Outcome

Chosen option: **portable baseline + delta file**, because it needed the smallest change to the
existing config, and cargo already supports it: `target.<triple>.rustflags` entries from multiple
config sources (a file passed via `--config` and the checked-in `.cargo/config.toml`) join together
rather than one replacing the other.

`.cargo/config.toml`'s `[target.aarch64-unknown-linux-gnu]` block now carries no `target-cpu` flag —
that's the portable baseline. `.cargo/neoverse-512tvb.toml` adds only the delta:

```toml
[target.aarch64-unknown-linux-gnu]
rustflags = ["-C", "target-cpu=neoverse-512tvb"]
```

A tuned build passes both `--config .cargo/neoverse-512tvb.toml` and `--features
lore-base/neoverse-512tvb`. The Cargo feature is a second, independent switch: it's what
`lore-base/build.rs` reads (`CARGO_FEATURE_NEOVERSE_512TVB`) to decide whether to pass
`-mcpu=neoverse-512tvb` to the C build. It isn't inferred from `CARGO_CFG_TARGET_FEATURE` containing
`sve` — that would silently break if a future tuning enabled SVE without being this one, or vice
versa. The feature name says exactly which tuning it mirrors, and the two flags are meant to be
passed together.

`lore-server/Dockerfile` always adds the tuning flags for `arm64` too (keyed off buildx's
`TARGETARCH`), matching the published `-graviton` release image — building `linux/arm64` still
needs to happen under `--platform linux/amd64` emulation on non-Graviton hosts, unchanged from
before this decision.

### Consequences

- Good, because a portable `aarch64-unknown-linux-gnu` build now exists and runs on any arm64 Linux
  host.
- Good, because the tuning delta lives in one small file instead of being duplicated across every
  target block that might want it.
- Good, because the C and Rust builds can't drift apart silently — both are gated on the same
  explicit feature, not on a detection proxy.
- Bad, because a tuned build needs two flags passed together (`--config` and `--features`) instead
  of one; forgetting the feature flag builds Rust tuned but rpmalloc portable.
- Neutral, because the published release binary and Docker image are unaffected here: their build
  paths already tune for aarch64, so those outputs are the same as before this change.

## Pros and Cons of the Options

### Runtime CPU-feature dispatch

- Good, because it needs one binary, not two.
- Bad, because `target-cpu`'s scheduling/tuning model isn't a per-function ISA feature — it can't be
  toggled with `#[target_feature]`, so the compiler-scheduling half of the benefit is unreachable
  this way regardless.

### Per-target-triple config files, always explicitly selected

- Good, because each file is fully self-contained — no merge behavior to reason about.
- Bad, because every target triple needs its own file even when only one variant (aarch64) needs a
  choice at all, and every caller (local dev, every CI job) must remember to pass one explicitly;
  there's no default that "just builds."

### Portable baseline + delta file (chosen)

- Good, because the default (`cargo build --target aarch64-unknown-linux-gnu`, no extra flags) is
  the portable build — the safe choice needs no one to remember anything.
- Good, because the delta file only ever needs to hold the one flag that differs.
- Neutral, because it relies on cargo's config-merging behavior for `rustflags`, which is documented
  but not obvious without reading the reference.

## More Information

- `.cargo/config.toml` and `.cargo/neoverse-512tvb.toml`.
- `lore-base/build.rs` and the `neoverse-512tvb` feature in `lore-base/Cargo.toml`.
- `lore-server/Dockerfile`.
