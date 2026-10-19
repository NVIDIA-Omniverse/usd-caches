Up: [local cache](README.md)

# Hub Protobuf RPC

## Two contracts, one served

| | v1beta (current) | v1alpha (deprecated) |
|---|---|---|
| File | [`usd_caches/local_cache/v1beta/local_cache.proto`](../../../../local-cache/reference/hub-rpc/proto/nvidia/omniverse/usd_caches/local_cache/v1beta/local_cache.proto) | [`hub/v1alpha/hub.proto`](../../../../local-cache/reference/hub-rpc/proto/nvidia/omniverse/hub/v1alpha/hub.proto) |
| Package | `nvidia.omniverse.usd_caches.local_cache.v1beta` | `nvidia.omniverse.hub.v1alpha` (header comment: "This package has been deprecated and will be removed in the next release.") |
| Compiled into Hub | **Yes**, the only proto in `hub-rpc/build.rs` `compile_protos`. `hub-rpc/src/hub-rpc.rs` includes `nvidia.omniverse.usd_caches.local_cache.v1beta.rs`. | No. It ships in the source tree only. |
| Served on | `transport.tcp.ext_port`, default **14093** (`hub/src/rpc/server_protobuf.rs` decodes every frame as v1beta `proto::Request`) | No separate listener. A v1alpha client talking to port 14093 is decoded with the v1beta schema. |
| Release note | `docs/index.rst` 2.4.0: "The Protobuf interface has been moved to nvidia.omniverse.usd_caches.local_cache and is now in beta." `CHANGELOG.md` 2.4.0: "Cleanup Hub protos and move to v1beta". | Added in 2.2.0 (`docs/index.rst`). |

Port 14092 (`rpc_port`) speaks **MessagePack** with the Rust enums in `hub-rpc/src/hub-rpc.rs`, not Protobuf (`hub/src/rpc/server_msgpack.rs`).

**v1alpha → v1beta differences** (a `diff` of the two files after removing the package names):

| v1alpha | v1beta | Wire effect |
|---|---|---|
| `HubVersionInfoParams {}` | `ServiceVersionInfoParams {}` | Same field number in the oneof (`hub_version_info = 2` / `service_version_info = 2`), so it is wire-identical. |
| `HubVersionInfoResult { VersionInfo version_info = 1; }` | `ServiceVersionInfoResult { VersionInfo version_info = 1; }` | Wire-identical (`ResponseResult` field 2). |
| `BlockConnectItem.optional string extension = 2` | removed (numbers 1, 3, 4 are kept) | Hub skips field 2 as unknown, so it is ignored. |
| `BlockPutItem.optional string extension = 4` | removed | Ignored. In v1alpha it was already ignored for blocks by design (`dispatcher.rs` NOTE: "fully ignoring the extension for block operations"). |
| Comments, `go_package`, `java_outer_classname` (`HubProto` → `LocalCacheProto`) | — | None. |

Every other message is identical in both files: `ResultCode`, `SemVer`, `Interface`, `InterfaceVersion`, `VersionInfo`, `ClientConnectParams`, `ClientConnectResult`, `CacheAsideVersionInfoParams`, `CacheAsideVersionInfoResult`, `CacheAsideConnectParams`, `FileConnectItem`, `CacheAsideConnectResult`, `CacheAsideOpenForReadParams`, `CacheAsideOpenForReadResult`, `CacheAsideOpenForWriteParams`, `CacheAsideOpenForWriteResult`, `CacheAsideCloseParams`, `CacheAsideCloseResult`, `CacheAsideRemoveParams`, `CacheAsideRemoveResult`, `BlockCacheConnectParams`, `BlockCacheConnectResult`, `BlockCacheGetParams`, `BlockCacheGetResult`, `BlockGetResult`, `BlockInfo`, `BlockLocation`, `BlockCacheCloseParams`, `BlockCacheCloseResult`, `BlockCachePutParams`, `BlockCachePutResult`, `BlockPutResult`, `BlockCacheCommitParams`, `BlockCacheCommitResult`, `BlockCacheAbortParams`, `BlockCacheAbortResult`, `BlockCacheStatParams`, `BlockMetadata`, `BlockStatResult`, `BlockCacheStatResult`, `ErrorInfoResult`, `RequestParams`, `ResponseResult`, `Request`, `Response`.

So existing v1alpha stubs keep working against current Hub. That follows from the field numbers and the fact that protobuf package names are not on the wire. No test in `hub/tests/` exercises v1alpha stubs. Write new clients against v1beta. The published docs (`docs/index.rst` "Protobuf Definitions and Examples", `docs/PROTOBUF_RPC.md`, `reference/README.md`) still name the v1alpha path; see [troubleshooting.md](troubleshooting.md#known-source-defects).

The rest of this page uses v1beta names. For v1alpha, read `hub_version_info`/`HubVersionInfo*` where it says `service_version_info`/`ServiceVersionInfo*`.

**Server:** `local-cache/reference/hub/src/rpc/server_protobuf.rs` (transport) and `hub/src/rpc/dispatcher.rs` (semantics), with the engine in `hub-cache/src/block/cache.rs`. Narrative docs: `local-cache/reference/docs/PROTOBUF_RPC.md` and `docs/index.rst` "Protobuf RPC Interface". This is not gRPC: there is no service definition and no HTTP/2.

## Framing and connection workflow

- TCP to `transport.tcp.host:ext_port` (default `127.0.0.1:14093`).
- Each message is a **4-byte big-endian unsigned length** (payload bytes, excluding the header) followed by a serialized `Request` (client → Hub) or `Response` (Hub → client). Frames over **32 MiB** are rejected: the codec errors and the server logs `Error reading message from client` (tokio `LengthDelimitedCodec` with `max_frame_length(32 MiB)`).
- Keep **one persistent connection per client session**. Handles, the client identity, and open reads and writes belong to the connection. On disconnect, reads are closed and writes are abandoned (see [overview.md](overview.md#per-connection-state)).
- `request_id` is echoed in the `Response`. It "should be unique across all pending requests". This server answers requests on a connection one at a time and in order. The Python async example comment says Hub "may process requests out of order", so match on `request_id` anyway.

Session order:
1. Optionally send `ServiceVersionInfo` and `CacheAsideVersionInfo`. These work without connecting.
2. Send `ClientConnect` with a name and a UUID string. Every other operation fails with `error_info` `Unknown client uuid. Client must call connect() first!` until you do.
3. After a client restart, reclaim state with `CacheAsideConnect`/`BlockCacheConnect`, passing the same `client_uuid` and the handles you held before.
4. Run cache-aside or block-cache operations.

## Envelope

| Message | Fields |
|---|---|
| `Request` | `uint64 request_id = 1`; `RequestParams params = 2` |
| `RequestParams` | `oneof params`: `client_connect=1`, `service_version_info=2`, `cache_aside_version_info=3`, `cache_aside_connect=4`, `cache_aside_open_for_read=5`, `cache_aside_open_for_write=6`, `cache_aside_close=7`, `cache_aside_remove=8`, `block_cache_connect=9`, `block_cache_get=10`, `block_cache_put=11`, `block_cache_close=12`, `block_cache_commit=13`, `block_cache_abort=14`, `block_cache_stat=15` |
| `Response` | `uint64 request_id = 1`; `ResponseResult result = 2` |
| `ResponseResult` | `oneof result`: the same names and numbers 1–15 with `*Result` types, plus `ErrorInfoResult error_info = 16` |
| `ErrorInfoResult` | `string message = 1`. Returned instead of the typed result for: an unset `params` oneof (`unrecognized message type`), not connected, an unparseable `client_uuid`, or internal errors such as a database flush failure. |

## Common types

| Type | Definition | Server values |
|---|---|---|
| `ResultCode` (enum) | `SUCCESS=0`, `ERROR_ABANDONED=1`, `ERROR_ALREADY_EXISTS=2`, `ERROR_TOO_LARGE=3`, `ERROR_HANDLE_NOT_FOUND=4`, `ERROR_INTERNAL=5`, `ERROR_NOT_FOUND=6`, `ERROR_PENDING_WRITE=7`. All are prefixed `RESULT_CODE_`. | Engine-error mapping (`dispatcher.rs` `convert_block_cache_error`): BlockAbandoned→ABANDONED, KeyAlreadyExists→ALREADY_EXISTS, KeyNotFound→NOT_FOUND, KeyNotOpen→HANDLE_NOT_FOUND, KeyOpenForPut→PENDING_WRITE, KeyTooLarge→TOO_LARGE, anything else (empty key, size 0, I/O, missing extension)→INTERNAL. |
| `SemVer` | `uint64 major=1, minor=2, patch=3` | — |
| `Interface` (enum) | `INTERFACE_UNKNOWN=0`, `INTERFACE_CACHE_ASIDE=1`, `INTERFACE_BLOCK_CACHE=2` | — |
| `InterfaceVersion` | `Interface interface=1`, `uint32 version=2` | — |
| `VersionInfo` | `SemVer software_version=1`, `repeated InterfaceVersion interface_versions=2` | — |

## Service-level

| Request → Result | Fields | Behavior |
|---|---|---|
| `ClientConnectParams` → `ClientConnectResult{}` | `client_name=1` (string), `client_uuid=2` (UUID string) | Sets this connection's identity. The UUID is stored in write records and is how an abandoned write is reclaimed later. A bad UUID string returns `error_info`. Calling it again overwrites the identity. |
| `ServiceVersionInfoParams{}` → `ServiceVersionInfoResult` | `version_info=1` | `software_version` is the Hub semver, `2.4.0`. `interface_versions` = `[CACHE_ASIDE v1, BLOCK_CACHE v1]` (`server_protobuf.rs` `VERSION_INFO`). |

## Cache-aside (whole files)

The client owns the fetch. It reads, gets `NOT_FOUND`, fetches from upstream, writes into the path Hub returns, and closes to commit. The cache key is `key` + `extension`. `provider` is ignored.

| Request → Result | Fields | Behavior |
|---|---|---|
| `CacheAsideVersionInfoParams{}` → `CacheAsideVersionInfoResult` | `SemVer version=1` | Always `1.0.0`. |
| `CacheAsideOpenForReadParams` → `CacheAsideOpenForReadResult` | in: `key=1`, `provider=2`, `extension=3` (may be empty); out: `result_code=1`, `optional local_file=2`, `optional handle=3` | Hit: `SUCCESS`, an absolute path `<cache>/hub-files/<XXX>/<016X>.<ext>`, and a handle (the read reference count goes up). Miss or abandoned: `ERROR_NOT_FOUND`. Being written by anyone: `ERROR_PENDING_WRITE`. Read and mmap the file, then `Close`. Counts toward the `cache_hits`/`cache_misses` stats. |
| `CacheAsideOpenForWriteParams` → `CacheAsideOpenForWriteResult` | in: `key=1`, `provider=2`, `extension=3`, `size=4`; out: `result_code`, `local_file`, `handle` | Reserves a new empty file and returns its path. Write the content there, then `Close` to commit. Errors: `size=0` → `ERROR_INTERNAL`; `size > max_size` → `ERROR_TOO_LARGE`; already committed or open for read → `ERROR_ALREADY_EXISTS`; open for write → `ERROR_PENDING_WRITE`. An open-for-write on an **abandoned** key succeeds for any client. It reuses the old data location only if the UUID and size match and the file "exists". That `exists()` check uses the *relative* path, so it is resolved against Hub's CWD (`cache.rs` `put1`). A new location is normally allocated while the index record keeps the old one. This is suspect; see [troubleshooting.md](troubleshooting.md#known-source-defects). |
| `CacheAsideCloseParams` → `CacheAsideCloseResult` | in: `handle=1`; out: `result_code` | Read handle: drops the reference. Write handle: commits, with the recorded size set to the file's actual length on disk. Unknown handle → `ERROR_HANDLE_NOT_FOUND`. |
| `CacheAsideRemoveParams` → `CacheAsideRemoveResult` | in: `handle=1`; out: `result_code` | Closes the handle and deletes the index record if it held the last reference, or immediately for a write in progress. The data file is reclaimed by the next GC pass. Removing a committed entry does **not** decrement `cache_entries`/`cache_size` until Hub restarts (defect 28 in [troubleshooting.md](troubleshooting.md#known-source-defects)). |
| `CacheAsideConnectParams` → `CacheAsideConnectResult` | in: `repeated FileConnectItem files=1` (`key=1`, `provider=2`, `extension=3`, `handle=4`, `open_for_write=5`); out: `result_code=1` (always `SUCCESS`), `repeated file_result_codes=2` (one per item) | Re-registers handles after a reconnect. For a read item, the entry must still be committed (`ERROR_NOT_FOUND` otherwise, `ERROR_PENDING_WRITE` if it is being written). For a write item, the entry must be abandoned by the **same** UUID (`ERROR_ABANDONED` if another UUID wrote it, `ERROR_ALREADY_EXISTS` if it is already committed). Reusing a handle number that is already open on this connection triggers a server `assert!` panic, which drops the connection. |

## Block cache (derived data)

Blocks are packed into shared data files. Every location is `(path, offset, size)` relative to the `base_path` in the result (`<cache>/hub-files`). Block keys are raw `bytes`. Each call takes a batch and returns one result per item, in order.

| Request → Result | Fields | Behavior |
|---|---|---|
| `BlockCachePutParams` → `BlockCachePutResult` | in: `repeated BlockPutItem blocks=1` (`bytes key=1`, `uint64 size=2`, `uint64 rank=3`); out: `repeated BlockPutResult results=1` (`result_code=1`, `optional BlockInfo block=2`), `string base_path=2` | Reserves space. Write `size` bytes at `base_path/path` + `offset` (the file is preallocated, so open it read-write), then `Commit` or `Abort`. Error codes are the same as for open-for-write. `rank` is accepted but **not used** (records store `rank: 0`, and eviction is LRU). |
| `BlockCacheCommitParams` → `BlockCacheCommitResult` | `repeated uint64 handles=1` → `repeated result_codes=1` | Marks the blocks committed and releases the handles. Unknown handle → `ERROR_HANDLE_NOT_FOUND`. |
| `BlockCacheAbortParams` → `BlockCacheAbortResult` | `handles` → `result_codes` | Releases the handles and **deletes** the pending index records (`cache.rs` `abort1_deprecated`: `index.remove` plus removal from the open-record map). The key is then `ERROR_NOT_FOUND` and can be put again. The reserved bytes are reclaimed only when no record references that data file and GC deletes it. Only a **disconnect** marks open puts *abandoned* (`dispatcher.rs` `Drop`). Unknown handle → `ERROR_HANDLE_NOT_FOUND`. |
| `BlockCacheGetParams` → `BlockCacheGetResult` | in: `repeated bytes keys=1`, `uint64 timeout_us=2`; out: `repeated BlockGetResult results=1` (`result_code`, `optional BlockInfo block`), `base_path=2` | Opens each key for read. Missing or abandoned → `ERROR_NOT_FOUND`; being written → `ERROR_PENDING_WRITE`. `timeout_us` is carried through the conversion layer but **ignored**: the call never waits. Release each handle with `Close`. |
| `BlockCacheCloseParams` → `BlockCacheCloseResult` | `handles` → `result_codes` | Releases read handles. |
| `BlockCacheStatParams` → `BlockCacheStatResult` | in: `keys=1`, `timeout_us=2` (ignored); out: `repeated BlockStatResult results=1` (`result_code`, `optional BlockMetadata metadata`) | Returns metadata for committed blocks only (uncommitted → `ERROR_NOT_FOUND`). Opens nothing, but queues an LRU touch. Doesn't change the hit and miss stats. |
| `BlockCacheConnectParams` → `BlockCacheConnectResult` | in: `repeated BlockConnectItem blocks=1` (`bytes key=1`, `uint64 handle=3`, `bool open_for_put=4`); out: `repeated result_codes=1` | Reconnect semantics are the same as `CacheAsideConnect`. |
| `BlockInfo` / `BlockLocation` | `handle=1`, `location=2` / `path=1` (relative), `id=2` (data file id), `offset=3`, `size=4` | — |
| `BlockMetadata` | `last_accessed=1` (epoch s), `accesses=2`, `size=3` | `last_accessed` is updated asynchronously by the LRU task. `accesses` over-counts: the LRU task never clears its batch buffer, so each batch re-applies every earlier touch (`hub-cache/src/block/cache.rs`). |

**Batch caveat:** the `get`, `put`, `close`, `commit`, `abort`, and `stat` handlers record the first element (`params.keys[0]`, `params.handles[0]`, or `params.requests[0]`) in a `debug`-level tracing span (`dispatcher.rs` `#[instrument]`). An **empty list** therefore can panic the connection task when that debug span is enabled. Reproduced on the published 2.3.0 container: with `RUST_LOG=debug`, an empty batch to each of the six handlers closed the connection (index out of bounds in `dispatcher.rs`), and Hub kept running. With the image defaults, empty batches returned empty results, even with `max_level = "debug"` in `hub.toml`. For 2.4.0 this is inferred from the same attributes. Never send empty batches.

## Runnable examples

Prerequisites: a running Hub in shared mode (`hub --mode=shared`, see [install-and-run.md](install-and-run.md)) and Python 3.9+. These commands generate stubs into a scratch directory outside the repo.

```bash
python3 -m venv /tmp/hubpb && . /tmp/hubpb/bin/activate
pip install protobuf grpcio-tools
cd local-cache/reference/hub-rpc
python -m grpc_tools.protoc \
  -I proto/nvidia/omniverse/usd_caches/local_cache/v1beta \
  --python_out=/tmp/hubpb local_cache.proto          # -> /tmp/hubpb/local_cache_pb2.py
cp examples/python/*.py /tmp/hubpb/
cd /tmp/hubpb
```

The `-I` must point at the proto's own directory, because the examples `import local_cache_pb2` as a top-level module. A system `protoc` works too: `protoc -I <same dir> --python_out=/tmp/hubpb local_cache.proto`. The generated code needs a `protobuf` runtime at least as new as the generator.

### Repo examples (`local-cache/reference/hub-rpc/examples/python/`)

- `python simple_client.py`: blocking socket on `127.0.0.1:14093`. It prints the version result, connects, writes a random 10-character key with 100 bytes through cache-aside, and reads it back. On success it exits 0 silently after the print. A failure raises `AssertionError` or a connection error. Expected output:

  ```text
  service_version_info {
    version_info {
      software_version {
        major: 2
        minor: 4
      }
      interface_versions {
        interface: INTERFACE_CACHE_ASIDE
        version: 1
      }
      interface_versions {
        interface: INTERFACE_BLOCK_CACHE
        version: 1
      }
    }
  }
  ```

  `patch: 0` is omitted by the proto3 text format, and the version numbers depend on the build.
- `python async_client.py`: the same flow with asyncio pipelining, and five concurrent reads of one key. The output matches above. On its error and shutdown paths it iterates `pending` without `.items()` and calls `asyncio.sleep(1)` without `await`, but the happy path is unaffected.

Both examples send `cache_aside_open_for_write` and `open_for_read` without an extension. Hub accepts an empty extension: the key becomes `<key>\0` and the file has no suffix.

### Full cache-aside and block-cache demo

Verification status: this script, `simple_client.py`, and `async_client.py` were compiled and run successfully against a scratch mock server. The mock was built from the same generated v1beta stubs (grpcio-tools 1.84.0, protobuf 7.36.2) and mimics Hub's framing and result codes. They were **not** run against a live Hub binary, because building Hub was out of scope. A v1alpha↔v1beta round trip (`hub_version_info` ↔ `service_version_info`, and `BlockPutItem.extension` skipped) also passed with real stubs. Save the script as `/tmp/hubpb/hub_pb_demo.py`.

```python
#!/usr/bin/env python3
"""Exercise Hub's Protobuf RPC (nvidia.omniverse.usd_caches.local_cache.v1beta) end to end."""
import json
import os
import socket
import struct
import sys
import urllib.request
import uuid

import local_cache_pb2 as pb

HOST = os.environ.get("HUB_HOST", "127.0.0.1")
PB_PORT = int(os.environ.get("HUB_PB_PORT", "14093"))
HTTP_PORT = int(os.environ.get("HUB_HTTP_PORT", "14090"))
RC = pb.ResultCode.Name


class Hub:
    """One persistent connection == one Hub client session (state is per connection)."""

    def __init__(self, host: str, port: int):
        self.sock = socket.create_connection((host, port))
        self.next_id = 0

    def call(self, name: str, msg):
        self.next_id += 1
        req = pb.Request(request_id=self.next_id)
        field = getattr(req.params, name)
        field.SetInParent()  # mark the oneof member present even when msg is empty
        field.MergeFrom(msg)
        data = req.SerializeToString()
        self.sock.sendall(struct.pack(">I", len(data)) + data)  # 4-byte big-endian length
        (length,) = struct.unpack(">I", self._recvn(4))
        resp = pb.Response()
        resp.ParseFromString(self._recvn(length))
        assert resp.request_id == req.request_id
        which = resp.result.WhichOneof("result")
        if which == "error_info":
            raise RuntimeError(f"{name}: {resp.result.error_info.message}")
        return getattr(resp.result, which)

    def _recvn(self, n: int) -> bytes:
        buf = bytearray()
        while len(buf) < n:
            chunk = self.sock.recv(n - len(buf))
            if not chunk:
                raise ConnectionError("Hub closed the connection")
            buf.extend(chunk)
        return bytes(buf)


def cache_aside(hub: Hub, key: str, data: bytes) -> None:
    ident = dict(key=key, provider="demo", extension="txt")
    r = hub.call("cache_aside_open_for_read", pb.CacheAsideOpenForReadParams(**ident))
    print(f"cache-aside read #1: {RC(r.result_code)}")
    if r.result_code == pb.RESULT_CODE_ERROR_NOT_FOUND:
        w = hub.call("cache_aside_open_for_write", pb.CacheAsideOpenForWriteParams(size=len(data), **ident))
        print(f"cache-aside open_for_write: {RC(w.result_code)} local_file={w.local_file}")
        with open(w.local_file, "wb") as f:
            f.write(data)
        c = hub.call("cache_aside_close", pb.CacheAsideCloseParams(handle=w.handle))
        print(f"cache-aside close (commit): {RC(c.result_code)}")
        r = hub.call("cache_aside_open_for_read", pb.CacheAsideOpenForReadParams(**ident))
        print(f"cache-aside read #2: {RC(r.result_code)}")
    elif r.result_code != pb.RESULT_CODE_SUCCESS:
        raise RuntimeError(f"open_for_read failed: {RC(r.result_code)}")
    with open(r.local_file, "rb") as f:
        assert f.read() == data
    c = hub.call("cache_aside_close", pb.CacheAsideCloseParams(handle=r.handle))
    print(f"cache-aside verified {len(data)} bytes at {r.local_file}; close: {RC(c.result_code)}")


def block_cache(hub: Hub, key: bytes, data: bytes) -> None:
    s = hub.call("block_cache_stat", pb.BlockCacheStatParams(keys=[key]))
    print(f"block stat #1: {RC(s.results[0].result_code)}")
    if s.results[0].result_code == pb.RESULT_CODE_ERROR_NOT_FOUND:
        p = hub.call("block_cache_put", pb.BlockCachePutParams(blocks=[pb.BlockPutItem(key=key, size=len(data), rank=0)]))
        pr = p.results[0]
        if pr.result_code != pb.RESULT_CODE_SUCCESS:
            raise RuntimeError(f"block put failed: {RC(pr.result_code)}")
        loc = pr.block.location
        print(f"block put: base_path={p.base_path} path={loc.path} offset={loc.offset} size={loc.size}")
        with open(os.path.join(p.base_path, loc.path), "r+b") as f:  # shared, preallocated data file
            f.seek(loc.offset)
            f.write(data)
        cm = hub.call("block_cache_commit", pb.BlockCacheCommitParams(handles=[pr.block.handle]))
        print(f"block commit: {[RC(x) for x in cm.result_codes]}")
    g = hub.call("block_cache_get", pb.BlockCacheGetParams(keys=[key]))
    gr = g.results[0]
    if gr.result_code != pb.RESULT_CODE_SUCCESS:
        raise RuntimeError(f"block get failed: {RC(gr.result_code)}")
    loc = gr.block.location
    with open(os.path.join(g.base_path, loc.path), "rb") as f:
        f.seek(loc.offset)
        assert f.read(loc.size) == data
    cl = hub.call("block_cache_close", pb.BlockCacheCloseParams(handles=[gr.block.handle]))
    print(f"block get verified {loc.size} bytes; close: {[RC(x) for x in cl.result_codes]}")
    s = hub.call("block_cache_stat", pb.BlockCacheStatParams(keys=[key]))
    m = s.results[0].metadata
    print(f"block stat #2: {RC(s.results[0].result_code)} size={m.size} accesses={m.accesses}")


def main() -> None:
    key = sys.argv[1] if len(sys.argv) > 1 else f"demo-{uuid.uuid4()}"
    hub = Hub(HOST, PB_PORT)
    v = hub.call("service_version_info", pb.ServiceVersionInfoParams()).version_info
    sv = v.software_version
    ifaces = ", ".join(f"{pb.Interface.Name(i.interface)} v{i.version}" for i in v.interface_versions)
    print(f"service: {sv.major}.{sv.minor}.{sv.patch} [{ifaces}]")
    ca = hub.call("cache_aside_version_info", pb.CacheAsideVersionInfoParams()).version
    print(f"cache-aside protocol: {ca.major}.{ca.minor}.{ca.patch}")
    hub.call("client_connect", pb.ClientConnectParams(client_name="hub_pb_demo", client_uuid=str(uuid.uuid4())))
    cache_aside(hub, key, f"cache-aside payload for {key}\n".encode())
    block_cache(hub, f"blk-{key}".encode(), f"block payload for {key}".encode())
    with urllib.request.urlopen(f"http://{HOST}:{HTTP_PORT}/api/1.0/cache/stats") as resp:
        stats = json.load(resp)
    print("stats:", {k: stats[k] for k in ("cache_entries", "cache_size", "cache_hits", "cache_misses")})


if __name__ == "__main__":
    main()
```

Run it twice with the same key: `python hub_pb_demo.py mykey`. For an exclusive or non-default Hub, read the ports from the running instance, with `C` set to its `cache.path`:

```bash
export HUB_HTTP_PORT=$(cat "$C/hub.port")
export HUB_PB_PORT=$(curl -s "http://127.0.0.1:$HUB_HTTP_PORT/api/1.0/service/config" | jq .transport.tcp.ext_port)
```

`hub --write-config hub-config.json` with the same `cache.path` writes the same JSON. Don't use `hub --show-config`: it prints the settings a new process would use (ports `0` in exclusive mode), not the running instance's.

Expected first run (paths and ids vary):

```text
service: 2.4.0 [INTERFACE_CACHE_ASIDE v1, INTERFACE_BLOCK_CACHE v1]
cache-aside protocol: 1.0.0
cache-aside read #1: RESULT_CODE_ERROR_NOT_FOUND
cache-aside open_for_write: RESULT_CODE_SUCCESS local_file=/home/u/.cache/ov/cache/hub/hub-files/001/0000000000000001.txt
cache-aside close (commit): RESULT_CODE_SUCCESS
cache-aside read #2: RESULT_CODE_SUCCESS
cache-aside verified 30 bytes at /home/u/.cache/ov/cache/hub/hub-files/001/0000000000000001.txt; close: RESULT_CODE_SUCCESS
block stat #1: RESULT_CODE_ERROR_NOT_FOUND
block put: base_path=/home/u/.cache/ov/cache/hub/hub-files path=000/0000000000000000 offset=0 size=23
block commit: ['RESULT_CODE_SUCCESS']
block get verified 23 bytes; close: ['RESULT_CODE_SUCCESS']
block stat #2: RESULT_CODE_SUCCESS size=23 accesses=N
stats: {'cache_entries': N+2, 'cache_size': ..., 'cache_hits': H+2, 'cache_misses': M+1}
```

How to tell it worked:
- The script exits 0. Each `assert` checks that the bytes Hub points to are the bytes written.
- On the **second run with the same key**, `read #1` and `stat #1` return `SUCCESS` and no write or put happens. `cache_hits` grows by 2 and `cache_misses` by 0. This shows the entry persisted across connections. It also persists across Hub restarts, because the index is in sled.
- Stats arithmetic (`hub-cache/src/block/cache.rs` `get1`): every open-for-read or block get counts an access, and each one found counts a hit. `misses = accesses − hits`. Stat calls don't count. Don't read `accesses` in `BlockMetadata` as an access count; it over-counts.

Rust and C clients: `cargo run -p hub-client --example client` (see [build.md](build.md)) uses `hub_client::Client` in exclusive mode. The C API (`hub_initialize`, `hub_get_cache_aside`, `hub_get_block_cache`, ...) is generated into `target/<profile>/hub_client_bindings.h` (`local-cache/reference/hub-client/build.rs`, `src/hub_client_c.rs`).
