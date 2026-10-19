<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# OVDC

This is the repo for the Key-Value store. OVDC is a persistent and distributed
key-value store. The current storage engine is RocksDB. The storage engine is
subject to change.

The goal of this project is to provide a configurable and simple key-value
store. The Rocks storage engine allows for the deployment to be customized to
the needs of the application.

The current implementation requires that keys be at most 32 bytes in size.
*This is not a strict requirement.* This is subject to change.

This store should be considered when the application:
1. Does not require range scans;
1. Has more data than can easily fit in system memory;
1. Can handle data loss;
1. Does not require consistency when mutating data;
1. Data should be evicted based on an approximate LRU;
1. Requires only a PUT/GET operations.

It is possible to configure the store to have system memory dedicated to
serving key-value pairs. This will, in a sense, make the store act like a
memory cache. Writes to the system first go to memory, then to disk. Individual
puts will not be constrained to the speed of the storage under certain
conditions. This makes it possible to have performance comparable to an
in-memory cache such as memcached or Redis. This is only true under certain
configuration and with certain requirements.

Multi-node deployments partition the key space across all the nodes. The nodes
in a deployment can be aware of each other and if configured correctly, will
"gossip". Currently the system does nothing when a node is failed or failing.
This is subject to change. Few guarantees are made with regard to the
consistency of data. The cache is allowed to lose data and if a pair is
mutated, the store is allowed to return any version of the data stored at the
key. The application is responsible for dealing with this problem.

# Functions

**GET** Get previously stored keys. If a key is expired it is not returned.

**PUT** Put a key value pair or mutate existing data. Each pair can have an
assigned TTL. Currently a group of keys with the same TTL in a multi-node
cluster are not guaranteed to be removed "at the same time". That is, a subset
of the keys may still be served when others become unavailable. This is subject
to change. An expired key is not immediately removed. The server will not vend
the data but the key is later removed during GC.

**TOUCH** This operation acts much like an "exists" and it returns a count for
the number of keys that were found. Instead this function is called "touch"
because it updates the last used timestamp for a key. This allows an
application to signal to the cache that a pair should not be garbage collected.

# Garbage Collection

Each node operates an independent garbage collection (GC) service. When the
capacity of a node is below a configurable threshold (`minFreeCapacity`, 40% in
the reference config), GC will begin. The goal of GC is to delete a
pre-configured percentage of the entire keyspace (`deleteKeyspaceQuantile`, 60%
in the reference config). If there are 1,000 keys when GC begins then ~600 keys
will be deleted.

Keys are removed with an LRU policy. If a key is *touch*ed or used in a *get*
then the last used time is updated. GC first scans the database and removes
expired data. During this scan a histogram is created with all access times.
After the scan the histogram is used to choose a time floor. Any time stamp
older than the floor is removed.

The cache is optimized to reduce the number of access events that need to be
recorded. This system works regardless of the chosen storage engine. It works
by coalescing access events in a separate log. Eventually, the logs are
combined and pruned then applied to the storage engine.

# Native development

Run the commands below from this project’s root. In the `NVIDIA-Omniverse/usd-caches`
export, first change into `shared-derived-cache/reference/`; in a standalone checkout,
use the repository root. The Rust workspace is in `rust/`; its `rust-toolchain.toml` pins the compiler.
Cargo downloads dependencies from crates.io and the public rust-rocksdb Git
repository. The first build compiles RocksDB and can take several minutes.
Allow several gigabytes of free disk space for build artifacts.

## Prerequisites

Install [Rust through rustup](https://rustup.rs/), with Cargo's bin directory on
`PATH`. Also install Git, Make, a C/C++ compiler, CMake, libclang, and `protoc`.
The native prerequisites come from the RocksDB and gRPC build scripts; see
[the upstream RocksDB wrapper's dependencies](https://github.com/rust-rocksdb/rust-rocksdb#dependencies).

On Ubuntu 24.04:

```sh
sudo apt-get update
sudo apt-get install -y build-essential git curl ca-certificates cmake clang libclang-dev protobuf-compiler pkg-config
```

On macOS, install Xcode Command Line Tools and [Homebrew](https://brew.sh/), then:

```sh
xcode-select --install
brew install cmake llvm protobuf
export LIBCLANG_PATH="$(brew --prefix llvm)/lib"
export DYLD_LIBRARY_PATH="$LIBCLANG_PATH${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"
```

Skip `xcode-select --install` if the command line tools are already installed.
Keep the libclang exports in the terminal used for Cargo; Homebrew's LLVM is
not on the default library search path. The Makefile forwards `LIBCLANG_PATH`
to Cargo because macOS system Make can strip inherited `DYLD_*` variables.
If Linux bindgen cannot find libclang, set `LIBCLANG_PATH` to your installed LLVM library directory.

Install the pinned toolchain and check the protobuf compiler:

```sh
(cd rust && rustup show active-toolchain && rustup component add clippy)
protoc --version
```

Rustup selects the pinned compiler when Cargo runs inside `rust/`. If a future
checkout adds Git submodules, initialize them with
`git submodule update --init --recursive`; this checkout has none.

## Build and check

The root Makefile runs Cargo inside the Rust workspace:

```sh
make build
make test
make lint
```

For an optimized binary, run `make release`. Without Make, use the equivalent
commands inside `rust/`:

```sh
cargo build --locked --bin main
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo build --locked --release --bin main
```

## Build and check with Docker

The Docker helpers build the binary and run workspace tests and Clippy
without installing the native compiler prerequisites on the host. They require
Docker with a running daemon and Buildx. Use a builder with the
`docker-container` driver; create one if needed:

```sh
docker buildx create --name ovdc-local --driver docker-container --use
```

Run the checks from this project's root:

```sh
./scripts/docker-check.sh build
./scripts/docker-check.sh test
./scripts/docker-check.sh lint
# Or run all three:
./scripts/docker-check.sh all
```

These helpers use the existing Dockerfiles and build a local Rust toolchain
image on public Ubuntu, avoiding private base images and registry credentials.
Build exports the binary; test and lint export reports and return the recorded
check status. Each check uses a fresh output directory and prints its
location. See [the Docker helper instructions](scripts/README.md) for output
files, platform selection, and builder setup.

To inspect the build configuration without a running daemon:

```sh
./scripts/docker-check.sh all --print
```

`--print` validates the Bake target configuration; it does not run the build or
checks. Native and Docker checks do not establish that a published OVDC image
or Helm deployment is publicly accessible.

## Run locally

The reference config reserves large cache budgets and enables OTLP exporters.
Create a small development config with OTLP export disabled:

```sh
sed -e 's/cacheSize = "32G"/cacheSize = "64M"/' \
    -e 's/blockCacheSize = "8G"/blockCacheSize = "64M"/' \
    -e 's/exporter = "otlp"/exporter = "none"/g' \
    rust/ovdc/src/config.toml > /tmp/ovdc-local.toml
make run CONFIG=/tmp/ovdc-local.toml ENV_DIR=/tmp/ovdc-local-data
```

The server listens on gRPC port 3010 and exposes Prometheus metrics on port
3051. Check metrics with `curl http://localhost:3051/metrics`. Stop the server
with Ctrl-C. `/tmp/ovdc-local-data` holds the database and can be reused for
another run; remove it only when you want to discard that development data.

The binary accepts an environment directory followed by an optional TOML
config path. If omitted, the config defaults to `./config.toml`; the embedded
config is written there if that file does not exist. A direct invocation after
`make build` is:

```sh
RUST_BACKTRACE=1 ./rust/target/debug/main /tmp/ovdc-local-data /tmp/ovdc-local.toml
```

`RUST_BACKTRACE=1` or `full` provides panic details. The config's `logLevel`
controls normal stdout verbosity; `RUST_LOG` is a fallback. Set `IP` only when
configuring gossip, using an address reachable by the other nodes.

JWT verification and TLS are disabled in the reference config. It accepts
unauthenticated requests; use it on a trusted development machine. Authentication
still expects NVIDIA-specific JWT claims, and its documentation and fixtures
need further public-release work. The chart's default image and pull secret
also need public-access verification. Native development does not require them;
for deployments, supply an image you can access and review the
[Helm chart configuration](docker/helm/README.md).

## PUT, GET, and TOUCH smoke check

Install [grpcurl](https://github.com/fullstorydev/grpcurl#installation) (on macOS,
`brew install grpcurl`). With the local server running, execute these commands
from the repository root. Protobuf `bytes` fields use base64: the key below is
`demo` and the value is `hello`.

```sh
grpcurl -plaintext -import-path rust/ovdc_grpc/proto -proto rpc.proto \
  -d '{"begin":{}} {"declare":{"key":{"key":"ZGVtbw=="},"size":"5","value":"aGVsbG8="}}' \
  localhost:3010 ovdc.KeyValue/Put
grpcurl -plaintext -import-path rust/ovdc_grpc/proto -proto rpc.proto \
  -d '{"keys":[{"key":"ZGVtbw=="}]}' localhost:3010 ovdc.KeyValue/Get
grpcurl -plaintext -import-path rust/ovdc_grpc/proto -proto rpc.proto \
  -d '{"keys":[{"key":"ZGVtbw=="}]}' localhost:3010 ovdc.KeyValue/Touch
```

PUT should succeed (its default-valued status may print as `{}`), GET should
return `value: "aGVsbG8="` and `size: "5"`, and TOUCH should report `touched: 1`.
PUT is a stream: its first message opens the stream and the second declares and
supplies the value. See [the protocol](rust/ovdc_grpc/proto/rpc.proto) for larger
values and TTL support.

See [CONTRIBUTING.md](CONTRIBUTING.md) for contribution and public-export checks.

# RocksDB

The current storage engine is RocksDB, a log-structured merge-tree (LSM). Writes land in an
in-memory table, which is flushed to an immutable sorted file (an SST) on disk. Those files are
then merged and rewritten in the background by *compaction*. Nothing is ever updated in place:
a put, a mutation and a delete are all appends, and compaction is what eventually reconciles
them and reclaims the space.

The store keeps its data in two column families: `default` holds the key rows, and `blobs`
holds the parts of values too large to store inline. Each family is compacted independently
and has its own memory tables, so they can be tuned separately and can stall separately.

Most RocksDB behaviour is configurable through the TOML configuration file. The defaults quoted
below are RocksDB's own; a deployment may override them.

## Level Compaction Design

Compaction is organized into levels. Leveled compaction gives each level a byte target that is
a fixed multiple of the level above it:

```
target(L1) = max_bytes_for_level_base        # default 256 MB
target(Ln) = target(L1) x multiplier^(n-1)   # default multiplier 10
```

L0 is the exception. It has no byte target, and is compacted on file count instead
(`level0_file_num_compaction_trigger`, default 4). Its files each come straight from a flushed
memory table, so their key ranges overlap one another. From L1 down, each level is a single
sorted run: files within a level do not overlap and are key-ordered, so a lookup reads at most
one file per level.

For a seven level tree (L0 through L6) at RocksDB's defaults, with 64 MB files:

| Level | Target            | Files @64 MB | Share of data |
| ----: | ----------------: | -----------: | ------------: |
| L0    | ~256 MB (4 files) | 4            | ~0.001%       |
| L1    | 256 MB            | 4            | ~0.001%       |
| L2    | 2.56 GB           | 40           | ~0.01%        |
| L3    | 25.6 GB           | 400          | ~0.09%        |
| L4    | 256 GB            | 4,000        | ~0.9%         |
| L5    | 2.56 TB           | 40,000       | ~9%           |
| L6    | 25.6 TB           | 400,000      | ~90%          |

On a log scale, where each block is one order of magnitude:

```
     | target      |
-----+-------------+--------------------------------
L0   |  256 MB     | ##            (4 files, OVERLAPPING)
L1   |  256 MB     | ##            +
L2   |  2.56 GB    | ####          |
L3   |  25.6 GB    | ######        | sorted runs,
L4   |  256 GB     | ########      | non-overlapping
L5   |  2.56 TB    | ##########    |
L6   |  25.6 TB    | ############  +
                     +- x10/level -+
```

The same figures to linear scale, which is what matters when sizing a node:

```
L0-L4 |                                          ~1% combined
L5    ####|                                      ~9%
L6    ########################################   ~90%
```

Four consequences follow from this shape:

**The bottom level is effectively the whole database.** Because each level is ten times the one
above it, the last level holds roughly 90% of the bytes. Compaction into the bottom level is the
expensive operation, and deleted data is only fully reclaimed once it reaches there.

**Write amplification is roughly the multiplier times the number of levels.** Each byte is
rewritten about once per level it descends through, and each rewrite merges it against about
ten times its own volume, so a seven level tree at defaults costs on the order of 60x. This is
the dominant background I/O cost of the store, and the main thing traded away when the
multiplier is tuned.

**Read amplification is roughly the number of levels.** A lookup for a key that is not present
may probe each L0 file individually, because they overlap and cannot be searched as a group,
plus one file per level below. Bloom filters reduce most of these to a filter check rather than
a block read.

**Level targets schedule work, they do not cap growth.** Exceeding a target is what causes
compaction to be scheduled; it does not stop the level from growing. Under sustained writes the
levels run over target, and that accumulated backlog is what eventually crosses the write stall
thresholds described below.

Note that recent RocksDB releases enable `level_compaction_dynamic_level_bytes` by default,
which reverses the arithmetic: level targets are derived from the actual size of the last level
and divided upward, leaving the upper levels empty until there is enough data to justify them.
A small database then occupies only L0 and the bottom level. This bounds space amplification
considerably more tightly, so the table above describes a fully grown store rather than what a
partially filled one looks like.

**What OVDC configures.** In `log` mode the default column family is given four levels rather
than seven, because the compaction filter that reclaims emptied rows only acts at the bottommost
level -- with seven, a row reaches the bottom only once there is enough data to fill the six
levels above it, and in this mode large values live in the blobs column family so that one is
meant to stay small. `periodic_compaction_seconds` is what walks rows down to the bottom on a
store that is not growing. An explicit `engine.cf.num_levels` overrides the default.

## Write Stalls

A write stall is RocksDB refusing, or deliberately slowing, incoming writes because the
background work needed to absorb them has fallen behind. It is not an error condition in the
engine; it is the engine's back-pressure mechanism. Writes are buffered in memory tables and
flushed to disk as SSTs, and those SSTs are then compacted. Both of those are background jobs.
If writes arrive faster than flush and compaction can retire them, the backlog would grow
without bound and exhaust memory or disk, so RocksDB puts a ceiling on the backlog and holds
writers at it.

Two severities exist. A **delay** throttles the write rate to a computed number of bytes per
second; the write still completes, just more slowly. A **stop** blocks writers entirely until
the condition clears. See the upstream
[Write Stalls](https://github.com/facebook/rocksdb/wiki/Write-Stalls) documentation.

In OVDC a stop is not absorbed by the server. It is surfaced to the client as a retryable
busy/unavailable condition, so back-pressure reaches the application rather than being
converted into unbounded memory growth inside the service. The application is expected to back
off and retry.

### What Causes a Stall

There are four independent triggers. Each is a separate backlog, and any one of them alone is
enough to stall the store.

| Trigger | Threshold | Backlog that grew |
| ------- | --------- | ----------------- |
| Memtable count | `cf.max_write_buffer_number` (24) | Flush cannot keep up with writes |
| L0 file count | `cf.level0_slowdown_writes_trigger`, `cf.level0_stop_writes_trigger` (24) | L0→L1 compaction cannot keep up with flushes |
| Pending compaction bytes | `cf.soft_pending_compaction_bytes_limit`, `cf.hard_pending_compaction_bytes_limit` | Compaction below L1 cannot keep up |
| Write buffer manager | `engine.writeBufferManagerSize` | Total memtable memory across all families exceeded its budget |

**Memtable exhaustion.** Writes land in an active memtable of `cf.write_buffer_size` (64 MB
here). When it fills, it is sealed and queued for flush and a fresh one is opened. If flushes
do not complete fast enough, the queue reaches `cf.max_write_buffer_number` and there is
nowhere to put the next write, so writes stop until at least one memtable has been fully
flushed. Metrics: `__rocksdb_stalls_stops_memtable_limit`,
`__rocksdb_stalls_delays_memtable_limit`.

**L0 file accumulation.** Every flush produces one L0 file, and L0 files overlap in key range,
so reads must consult all of them. Compaction merges L0 into L1 to keep the count low. If
flushes outpace that compaction, the file count climbs past the slowdown trigger (delay) and
then the stop trigger (stop). Metrics: `__rocksdb_stalls_stops_l0_file_count_limit`,
`__rocksdb_stalls_delays_l0_file_count_limit`.

**Pending compaction bytes.** As described above, level targets schedule work rather than cap
growth. RocksDB estimates the bytes of compaction still owed across the tree; once that
estimate crosses the soft limit writes are delayed, and at the hard limit they stop. This is
the trigger that catches a store whose *lower* levels are behind, which the L0 count alone
would not show. Metrics: `__rocksdb_stalls_stops_pending_compaction_bytes`,
`__rocksdb_stalls_delays_pending_compaction_bytes`.

**Write buffer manager.** When `engine.useWriteBufferManager` is on, all column families share
one memtable memory budget. Exceeding it triggers flushes, and if the manager is configured to
stall, writes across *every* family stop until a flush completes — not just the family that
grew. Metric: `__rocksdb_stalls_write_buffer_manager_limit_stops`.

Aggregates for all of the above are exported as `__rocksdb_stalls_total_delays` and
`__rocksdb_stalls_total_stop`. Stalls are tracked per column family, so `default` and `blobs`
report separately; the `blobs` family holds the large values and is generally the one that
stalls first.

The underlying cause of all four is the same: **flush and compaction are disk-bound, and the
store cannot ingest faster than the storage can retire the background writes.** Compaction
rewrites each byte roughly once per level it descends, so sustained ingest of X MB/s of client
data implies an order of magnitude more disk traffic than X. The knobs that move stall
thresholds — larger memtables, more of them, higher triggers — buy headroom for bursts. They do
not change the sustainable rate. Only faster storage, more background threads
(`engine.threadsLow`, `engine.db.max_background_jobs`), or a lower ingest rate does that.

### Storage Throughput Requirements

OVDC is typically deployed into a CSP environment where its data directory is a network
attached PersistentVolumeClaim rather than a local NVMe device. That distinction matters more
here than for most workloads, because an LSM tree multiplies every client byte into many disk
bytes: the initial flush, then a rewrite at each level of compaction. Read traffic from
compaction is on top of that. A volume that comfortably serves an application's foreground
throughput can still leave compaction permanently behind, and a permanently behind compactor
means permanent stalls.

> **Provision network attached PVCs for at least 800 MB/s sustained throughput.** This is the
> minimum suggested figure for any PVC backing an OVDC node. Below it, background compaction
> cannot retire the write amplification a normally loaded node generates, the backlog grows,
> and the store spends its time in write stalls regardless of how the memtable and L0 limits
> are tuned.

Note that many CSP volume types scale IOPS and throughput with provisioned capacity, or offer a
burst credit balance that is exhausted under sustained load and then drops to a much lower
baseline. Size for the sustained figure, not the burst figure. A node that performs well for
the first hour and then stalls continuously is the usual signature of an exhausted burst
balance.

## Block Cache

The block cache holds uncompressed SST blocks in memory so that reads served from it do not
touch storage. It is the main read-side memory structure in RocksDB and is distinct from the
memtables, which hold data that has not yet been written to disk, and from OVDC's own
`engine.cacheSize` value cache, which caches whole key-value pairs above the storage engine.

Data in an SST is stored as blocks of `engine.block.block_size` bytes (16 KB here). A read
locates the block that may hold the key, and either finds it in the cache or reads it from
disk and inserts it. The cache is LRU. Because the unit is
a block and not a key, one cached block can serve many nearby keys — which is worth little for
the random access pattern of a key-value store, but is exactly why index and filter blocks
belong in the cache.

Three kinds of block compete for the space:

- **Data blocks** — the key-value rows themselves.
- **Index blocks** — the map from key range to block offset within an SST. One lookup consults
  the index of every SST it probes.
- **Filter blocks** — bloom filters (`block.filter_policy = "bloomfilter:10"`, ~10 bits per key)
  that answer "this SST definitely does not contain this key" without a data read. On a store
  with a high miss rate these are what keep a negative lookup from costing one read per level.

Index and filter blocks are the higher-value residents: they are consulted on *every* lookup
against their SST, whereas a data block is only useful if that specific block is read again.
The configuration reflects this. `block.cache_index_and_filter_blocks` puts them in the cache
so they are accounted for rather than growing outside any budget;
`block.pin_l0_filter_and_index_blocks_in_cache` pins the L0 ones so that the most frequently
consulted files cannot be evicted; and `block.partition_filters` with
`index_type = "kTwoLevelIndexSearch"` splits large indexes and filters into partitions so a
lookup loads only the partition it needs instead of a whole multi-megabyte block.
`block.prepopulate_block_cache = "kFlushOnly"` inserts blocks into the cache as they are
written by a flush, so freshly written data is warm without a read to fault it in.

**Sizing.** `engine.blockCacheSize` allocates the LRU cache at startup. Note that as currently
wired this cache is only handed to the write buffer manager, which charges memtable memory
against it; it is not attached to the block based table factory, so the tables themselves fall
back to a RocksDB-allocated default cache per column family. Raising `blockCacheSize` therefore
widens the memtable memory charge today rather than the amount of SST data held in memory. A
`block_cache` entry in the table options, or attaching the shared cache to the factory, is what
would make this knob control read caching as its name implies.

Cache effectiveness is exported as `block_cache_hit` and `block_cache_miss`, with occupancy in
`rocksdb.block-cache-usage` against `rocksdb.block-cache-capacity` and the pinned share in
`rocksdb.block-cache-pinned-usage`. A hit rate that falls as the dataset grows usually means
the working set of index and filter blocks no longer fits, at which point every lookup starts
paying a storage read before it can even decide whether the key exists — which is also
additional load on the same PVC that compaction needs.

When sizing a node, the block cache, the memtables (`cf.write_buffer_size` x
`cf.max_write_buffer_number`, per column family) and the `engine.cacheSize` value cache are
separate budgets that must all fit in the container memory limit alongside the process itself.
With `engine.useWriteBufferManager` enabled the memtable memory is charged against the block
cache, so the two share one accounting rather than adding independently.
