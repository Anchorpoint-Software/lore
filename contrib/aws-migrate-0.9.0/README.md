# lore-aws-migrate

Migrates every fragment in an AWS immutable store onto the fragment state table, the layout Lore 0.9.0 reads.

Before the change, a fragment was described by its own DynamoDB row: flags and sizes in the metadata table, payload in S3. Now the fragment travels on the S3 object that carries the payload, and DynamoDB keeps only a state row recording that the hash exists. A store written the old way needs every object rewritten with its metadata attached and a state row published. That is what this tool does, driving the migrator in `lore-aws` over every scan segment of the metadata table.

It is a standalone crate, deliberately outside the Lore workspace: an operator builds and runs it against a store on their own schedule, and it is not part of what the workspace builds or tests. It reaches into the repository for `lore-aws` by path, restates the workspace's `quinn-proto` patch (a patch table belongs to a workspace root), and pins its dependency versions in a committed `Cargo.lock`.

## The store must not be serving traffic

**Take the servers out of service before starting a migration.** Put every node that reaches this store into maintenance mode, or otherwise make them unreachable to users, and leave them that way until the run finishes.

This is not caution about load. The migration reads a fragment's state, then spends a GET, a decompress, a recompress and a PUT before it writes. A write arriving inside that window is a race the tool cannot win, and one of the outcomes destroys data that was meant to stay destroyed: if an obliteration takes the hash while the migration is working on it, the payload is uploaded anyway and the tombstone is revived to `Stored`. The takedown is undone silently — payload back in the bucket, hash writable and deduplicated against again. Nothing in the tool or the store detects this after the fact.

A node runs in maintenance mode when `LORE_SERVER_MAINTENANCE=1` is set in its environment. It then serves only the environment and health endpoints, on every port it would normally expose, answering `UNAVAILABLE` so that load balancer and deployment probes see a node that is deliberately out of service rather than a refused connection. No storage, replication, or admin service is registered, so no client can write, obliterate, or read through it.

On the `contrib/aws` topology, set it on both task definitions — the edge proxies writes to the primary, so quiescing the primary alone is not enough — and confirm no task is still running the previous revision before starting:

```sh
aws ecs describe-services --cluster lore-cluster --services lore lore-edge \
  --query 'services[].deployments[].{taskDefinition:taskDefinition,running:runningCount}' \
  --region us-west-2
```

Take the nodes back out of maintenance mode only once the run reports success and the counts have been read (see [What the totals mean](#what-the-totals-mean)).

## Prerequisites

- A stable Rust toolchain with edition 2024 support (1.85 or later; built and tested on 1.97). No nightly needed.
- A checkout of this repository. The crate depends on `../../lore-aws` and `../../vendor/quinn-proto` by path, so it cannot be built on its own.
- Network access for the first build, unless the dependencies are already in the local cargo cache — then `cargo build --offline` works.
- AWS credentials, resolved by the standard chain: environment variables, a shared profile (`AWS_PROFILE`), or the instance or container role where the tool runs on EC2 or ECS.

## Build

```sh
cd contrib/aws-migrate-0.9.0
cargo build --release
```

The binary lands at `target/release/lore-aws-migrate`. A debug build works too, but a migration reads, decompresses, recompresses, and rewrites every payload in the store, so build it release.

Oodle payloads need the Oodle libraries, which are not in this repository. Without them every Oodle fragment is reported unreadable and left behind:

```sh
OODLE_LIB_DIR=/path/to/oodle cargo build --release --features oodle
```

## Run

A full migration is every segment of the metadata table, which is the default:

```sh
./target/release/lore-aws-migrate \
  --s3-bucket lore-fragments-abc123 \
  --fragments-table lore-fragments \
  --fragment-state-table lore-fragment-state \
  --fragment-metadata-table lore-metadata \
  --region us-west-2
```

On a deployment that never had a separate table — where the state rows and the legacy metadata rows share one table, told apart by shape — pass that one name to both `--fragment-state-table` and `--fragment-metadata-table`.

The migration reads the S3 object, the metadata table it falls back to, and the state table, and it writes the object and a state row. Associations are neither read nor written, so `--fragments-table` is there to name the store completely and is only checked for existence.

The four store arguments can come from the environment instead, which is what a container running this wants:

| Argument | Environment variable |
|----------|----------------------|
| `--s3-bucket` | `LORE_MIGRATE_S3_BUCKET` |
| `--fragments-table` | `LORE_MIGRATE_FRAGMENTS_TABLE` |
| `--fragment-state-table` | `LORE_MIGRATE_FRAGMENT_STATE_TABLE` |
| `--fragment-metadata-table` | `LORE_MIGRATE_FRAGMENT_METADATA_TABLE` |
| `--region` | `AWS_REGION` |

Left unset, the region falls back to whatever the environment or the active profile resolves.

Rehearse first. `--dry-run` reads and analyses every fragment, reports what each outcome would be, and writes nothing — so unlike a real run it is safe against a live store, and it is how the length of the maintenance window gets estimated before anyone books one:

```sh
./target/release/lore-aws-migrate ... --dry-run
```

Scale the run with `--total-segments` (how many parallel scans the table is divided into) and `--consumers` (fragments converted at once within a segment). To spread one migration over several machines, give every machine the same `--total-segments` and one `--segment` each:

```sh
# machine 0 of 4
./target/release/lore-aws-migrate ... --total-segments 4 --segment 0
```

Exit status is 0 only when every segment this invocation covered completed. Progress totals are logged every 30 seconds (`--progress-interval-secs 0` to silence them), and `RUST_LOG` sets the log level.

Four more knobs matter on a large or unhappy store, and `--help` lists the rest with their defaults:

| Argument | Default | What it is for |
|----------|---------|----------------|
| `--timeout-millis` | 30000 | One S3 or DynamoDB operation. A payload rewrite is one operation, so this bounds the largest fragment the migration can move — raise it if large fragments time out |
| `--max-retries` | 5 | Attempts at a failing scan page or fragment before it is given up on |
| `--retry-base-delay-ms` | 200 | Multiplied by the attempt number, capped at five seconds. Raise it when DynamoDB is throttling |
| `--scan-limit` | unset | Items per scan page, to hold discovery back from outrunning the conversions |

### Interrupting and resuming

The run is resumable, and repeating the same command is how it is resumed: a fragment that already has a state row is skipped. `Ctrl-C` stops between fragments rather than during one. A resumed run needs the servers held out of service exactly as the first one did — the hazard above is per write, not per run, so bringing nodes back between attempts reintroduces it.

Resuming is not free. Discovery keeps no checkpoint and starts the scan from the beginning, so a resumed run re-reads every row and spends a state lookup on each fragment it already migrated before it reaches new work.

A read or write that keeps failing past its retries stops the whole run, all segments, so the cause can be dealt with before more of the store is touched. A payload that no codec can recover is different: nothing can be done for it, so it is counted as `unreadable` and passed over, and the run carries on. Under `--dry-run` nothing stops the run at all — every failure is counted so one pass reports everything wrong with the store.

## What the totals mean

| Total | Meaning |
|-------|---------|
| `scanned` / `metadata_rows` | Rows read from the metadata table, and those that yielded a hash to convert |
| `maintained` | Codec was declared correctly and is not Oodle: payload re-uploaded unchanged to attach its metadata |
| `recompressed_oodle` | Oodle payload recompressed to Zstd |
| `recompressed_mismatch` | Declared codec disagreed with the stored bytes; recompressed to Zstd |
| `stored_uncompressed` | Recompression did not pay for itself, so the payload is stored uncompressed |
| `payloads_deduced` | Codec had to be found by probing rather than trusted |
| `already_migrated` | State row already present, nothing to do |
| `obliterated` | Legacy row carries an obliteration flag, so the fragment is skipped and gets no state row. **Expected to be zero — see below** |
| `oversized` | Fragment claims a size past the threshold and is not read, so it gets no state row |
| `unreadable` | No codec reproduced the hash; the payload cannot be recovered, so it gets no state row |
| `errored` | Conversion failed after its retries |

### Success is per segment, not per fragment

Exit status 0 means every segment this invocation covered reached the end of its scan. It does **not** mean every fragment is migrated. Three outcomes leave a fragment behind and still let the run report success:

- `unreadable` — no codec reproduced the hash. Built without the `oodle` feature against a store holding Oodle payloads, this is where all of them land, in the thousands, and the run still exits 0.
- `oversized` — the fragment claims a size past the threshold, so it is never read.
- `obliterated` — see below.

**Read the counts before reopening traffic.** A migration is complete when `unreadable`, `oversized` and `obliterated` are all zero; a zero exit status alone does not establish that, and there is no verify pass that does. Until those three are zero, the deployment still needs the metadata table configured (`dynamodb_fragment_metadata_table` on the server, `fragment_metadata_table` in `contrib/aws`), because the fragments left behind are readable only through it.

### A nonzero `obliterated` count means stop

These are not races — the servers are out of service — but pre-existing rows in the metadata table whose flags say the payload was deliberately destroyed. The migration skips them, which leaves them with no state row, and a hash with no state row reads as absent and is re-uploadable. A takedown that survives today only because the legacy table records it would quietly stop being one the moment that table is retired.

The design of the new layout assumes no such row exists. If the count is not zero, stop and work out where those rows came from before going any further; do not treat it as an outcome alongside `maintained`.

## Permissions

The credentials the tool runs under need more than the server's own task role:

- `dynamodb:Scan` on the metadata table — the server never scans, so its policy does not grant this
- `dynamodb:GetItem` on the metadata and state tables, `dynamodb:PutItem` on the state table
- `dynamodb:DescribeTable` on all three tables, and `s3:ListBucket` on the bucket, which the tool uses to fail early on a name that is wrong
- `s3:GetObject` and `s3:PutObject` on the fragment bucket

Point `--s3-endpoint-url` and `--dynamodb-endpoint-url` at a local stack to rehearse against one, adding `--s3-force-path-style` where the endpoint needs it.
