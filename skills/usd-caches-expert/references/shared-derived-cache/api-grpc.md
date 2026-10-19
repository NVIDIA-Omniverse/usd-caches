Up: [shared derived-data cache](README.md)

# OVDC gRPC API (`ovdc.KeyValue`)

Contract: `shared-derived-cache/reference/rust/ovdc_grpc/proto/rpc.proto` (`syntax = "proto3"; package ovdc;`). Server behavior: `shared-derived-cache/reference/rust/ovdc/src/grpc/{mod,get,put}.rs` and `rust/ovdc/src/lib.rs`. Tests: `shared-derived-cache/reference/rust/ovdc/src/tests.rs`. Below, `ref/` means `shared-derived-cache/reference/`.

## Service

```proto
service KeyValue {
    rpc Get(GetRequest) returns (stream GetUnorderedResponse);   // server streaming
    rpc Put(stream PutRequest) returns (stream PutResponse);     // bidirectional streaming
    rpc Touch(TouchRequest) returns (TouchResponse);             // unary
}
```

- Full method paths: `/ovdc.KeyValue/Get`, `/ovdc.KeyValue/Put`, `/ovdc.KeyValue/Touch`. Legacy `/ddcs.KeyValue/*` paths are rewritten to these (`ref/rust/ovdc/src/lib.rs` `rewrite_legacy_package`).
- `grpc.health.v1.Health` is served on the same port, with SERVING set for the KeyValue service and for `ddcs.KeyValue`. **No server reflection**, so tools need `-proto rpc.proto`.
- Default endpoint `:3010`, plaintext h2c unless `grpcTls.enabled`.

## Messages (copied field-for-field from rpc.proto)

| Message | Field | # | Type | Notes |
|---|---|---|---|---|
| `S32Key` | `u0` | 1 | `uint64` | Each u64 is converted to big-endian bytes and the four are concatenated into a 32-byte key |
| | `u1` | 2 | `uint64` | |
| | `u2` | 3 | `uint64` | |
| | `u3` | 4 | `uint64` | |
| `Key` | `oneof inner { fixed_key = 1; key = 2; }` | | `S32Key` / `bytes` | `key` is ≤32 bytes; shorter keys are zero-padded server-side |
| `GetRequest` | `keys` | 3 | `repeated Key` | "It is encouraged to sort the keys but not required." |
| `GetUnorderedResponse` | `tag` | 1 | `uint32` | Index of the key in the request (see [Key constraints](#key-constraints)) |
| | `value` | 2 | `bytes` | Partial or full value |
| | `size` | 3 | `uint64` | Total value size; reassemble per `tag` until `size` bytes have arrived |
| `PutOpen` | `ttl` | 2 | `optional uint64` | TTL in **milliseconds** for all keys in this stream |
| | `durability` | 3 | `optional Durability` | `Low = 0` (default, no WAL), `Normal = 1` (WAL) |
| `PutOpen.Durability` | `Low` / `Normal` | 0 / 1 | enum | |
| `PutDeclare` | `key` | 1 | `Key` | |
| | `size` | 2 | `uint64` | Exact total bytes that will be sent for this entry |
| | `value` | 3 | `bytes` | First (or only) part of the value |
| `PutRequest` | `oneof kind { begin = 1; declare = 2; value = 3; }` | | `PutOpen` / `PutDeclare` / `bytes` | `begin` must come first, exactly once |
| `PutResponse` | `status` | 1 | `PutStatus` | `PUT_OK = 0`, `ERROR_CONCURRENT_WRITE = 1` (never emitted by this server) |
| | `index` | 2 | `optional uint32` | Ordinal of the declared entry within the stream (0-based) |
| `PutResponse.PutStatus` | `PUT_OK` / `ERROR_CONCURRENT_WRITE` | 0 / 1 | enum | |
| `TouchRequest` | `keys` | 2 | `repeated Key` | |
| `TouchResponse` | `touched` | 1 | `uint32` | Number of keys that exist |
| | `matched` | 2 | `repeated uint32` | Indices of the keys that exist |

## RPC semantics

### Put (bidirectional stream)

Stream grammar (`ref/rust/ovdc/src/grpc/put.rs`):

```
begin(PutOpen)  ( declare(PutDeclare{key,size,value=part0})  value(bytes)* )*  <half-close>
```

- One stream may carry **many entries**. Each entry is a `declare` followed by `value` parts until exactly `size` bytes have been sent. The server replies with one `PutResponse{status: PUT_OK, index: n}` per entry, in order, and closes the response stream after the client half-closes.
- Each message must be ≤ `grpc.maxDecodingMessageSize` (default `5M`; the proto comment says "This limit is 4MB"). Split larger values across `value` messages. The repo helper uses 1,000,000-byte parts (`ref/rust/ovdc_grpc/src/lib.rs` `put_request`).
- `size > store.blobCutoff` (Helm `2500`, config.toml `8000`) takes the chunked-blob path: server chunks of `blobChunkSize`, and the part count is checked against the backend maximum **before** reading the body. Otherwise the value is stored in a single write.
- TTL and durability from `begin` apply to every entry in the stream. Durability `Normal` is applied only on the single-value path; the chunked path ignores `durability` (`write_declared_entry` destructures only `ttl`).
- Concurrent PUT of a key whose chunked write is in progress: the later writer's bytes are drained and it still receives `PUT_OK`. The first writer wins (`sink_value`; test `test_last_write_wins`).
- The whole stream is bounded by `grpc.putTimeout` (default 1 h, max 4 h). On expiry the server returns `DEADLINE_EXCEEDED`.

### Get (server stream)

- Keys are processed in request order. Missing or expired keys produce **no message** (test `test_get_with_missing`). Large values arrive as several messages with the same `tag` and `size`. The proto calls the stream "unordered", so key on `tag`, not on arrival order.
- Every GET logs an access for LRU, even for missing keys.
- If a key is being written concurrently, the stream ends with `ABORTED "value changed"`. Treat it as retryable or as a miss (test `test_threaded_concurrent_put`).

### Touch (unary)

- Checks existence, marks found keys as recently used (protecting them from GC), and returns `touched` and `matched` (`ref/rust/ovdc/src/grpc/mod.rs` `touch`; test `test_that_touch_can_touch_put`).
- Source does not document whether TOUCH counts an expired key that GC has not yet removed. GET does not return expired keys (`ref/README.md`).

## Status codes

| Code | Message (exact) | When (source) |
|---|---|---|
| `FAILED_PRECONDITION` | `stream must begin with valid \`Begin\` enum kind` | First PUT message is not `begin` |
| `FAILED_PRECONDITION` | `invalid: no message` / `invalid: missing kind` / `invalid: declare missing kind` | Empty stream or message without `kind` |
| `FAILED_PRECONDITION` | `stream must begin with \`Begin\` enum kind` | A non-`declare` message where a declare was expected, for example extra `value` parts |
| `FAILED_PRECONDITION` | `key not found` / `invalid key` | `declare.key` is unset, or longer than 32 bytes |
| `INVALID_ARGUMENT` | `value of N bytes needs P parts of C bytes, but this storage mode supports at most M` | Value too large for the backend (legacy modes: 254 parts) |
| `ABORTED` | `incorrect number of bytes received` / `not enough bytes received` | Byte count ≠ `size`, or the stream ended early on the chunked path |
| `ABORTED` | `put failed` | DB write failed, including staying busy through 60 retries (write stall) |
| `INTERNAL` | `write failure` | A chunk write could not be scheduled because the DB stayed busy |
| `DEADLINE_EXCEEDED` | `put exceeded the configured limit of Ns` | `grpc.putTimeout` |
| `ABORTED` | `value changed` | GET raced an in-progress write |
| `INTERNAL` | `internal db error` / `failed to fetch value` | GET storage error or a missing blob part |
| `ABORTED` | `write timeout` / `channel closed` | The server could not queue a response within `grpc.writeTimeout`, or the client went away |
| `INVALID_ARGUMENT` | `tenant: not a utf8 string` / `tenant: invalid tenant length` | Bad `tenant` metadata (>48 bytes) |
| `INVALID_ARGUMENT` | `authorization: not a utf8 string` / `authorization: <reason>` / `authorization: invalid length of tenant` | Bad JWT |
| `PERMISSION_DENIED` | `no jwt` | `grpcJwt.enabled` and `require` are set, but no token was sent |

Clients should treat `ABORTED`/`INTERNAL` write failures and `value changed` as **retry with backoff, or regenerate**. A write stall is back-pressure (`ref/README.md` "Write Stalls"). On the single-value path, if a client declares more bytes than it sends and then half-closes, the server keeps waiting until `putTimeout` (the `while value.len() < size` loop in `write_declared_entry_single`). Always send exactly `size` bytes.

## Key constraints

- A user key is ≤32 bytes (`bytes key`), or exactly 32 bytes via `fixed_key` (`ref/rust/ovdc_bytes/src/keys.rs`).
- **Zero-padding aliasing:** keys shorter than 32 bytes are padded with `\0`, and equality uses the padded array. `b"demo"` and `b"demo\0"` therefore address the same entry.
- In GET and TOUCH, keys longer than 32 bytes, and `Key` messages with no `inner`, are **silently dropped before indexing**. `tag` and `matched` then refer to positions in the filtered list and no longer line up with your request. Validate key length client-side.
- Prefer fixed-size, uniformly distributed keys such as content hashes. Any cross-pod partitioning is the client's job (see [overview.md](overview.md#multi-node-behavior)).

## Metadata

| Key | Value | Effect |
|---|---|---|
| `tenant` | UTF-8, ≤48 bytes | Namespace for all keys in the call, when `grpc.tenantFromMetadata = true` (default). Not authenticated. |
| `authorization` (or `jwt`) | `Bearer <JWT>` or raw `<JWT>` | When `grpcJwt.enabled`, verified. The tenant becomes `assertion.ncaId` and overrides `tenant`. See [configuration-server.md](configuration-server.md#jwt-claim-requirements). |
| `traceparent` and similar | W3C trace context | Used as the parent span when OTLP tracing is on (`ovdc_trace::span_extractor::apply_distributed_trace`) |

## grpcurl examples

Install [grpcurl](https://github.com/fullstorydev/grpcurl#installation). Run these from `shared-derived-cache/reference/` against a local server (see [deploy.md](deploy.md#run-locally-native)) or a port-forwarded pod. `bytes` fields are base64 and `uint64` fields are JSON strings. These are the README smoke check (`ref/README.md` "PUT, GET, and TOUCH smoke check") plus variants.

```bash
P=(-plaintext -import-path rust/ovdc_grpc/proto -proto rpc.proto)   # array: works in bash and zsh
T=localhost:3010

# PUT key "demo" (ZGVtbw==) = "hello" (aGVsbG8=)
grpcurl "${P[@]}" -d '{"begin":{}} {"declare":{"key":{"key":"ZGVtbw=="},"size":"5","value":"aGVsbG8="}}' $T ovdc.KeyValue/Put
# GET
grpcurl "${P[@]}" -d '{"keys":[{"key":"ZGVtbw=="}]}' $T ovdc.KeyValue/Get
# TOUCH
grpcurl "${P[@]}" -d '{"keys":[{"key":"ZGVtbw=="}]}' $T ovdc.KeyValue/Touch
```

Expected output: PUT prints `{"index": 0}`; `index` is an `optional` field the server always sets (`rust/ovdc/src/grpc/put.rs`), and the default-valued `status: PUT_OK` is omitted. The upstream README's "may print as `{}`" doesn't apply. GET prints `"value": "aGVsbG8="` and `"size": "5"` (`tag` 0 omitted). TOUCH prints `"touched": 1` and `"matched": [0]`. The README states the PUT/GET/TOUCH expectations. The `index`/`matched` rendering was checked with protobuf's JSON mapping, not against a live server.

Variants:

```bash
# PUT with 60 s TTL, WAL durability, value split across two messages ("hello " + "world")
grpcurl "${P[@]}" -d '{"begin":{"ttl":"60000","durability":"Normal"}}
  {"declare":{"key":{"key":"ZGVtbzI="},"size":"11","value":"aGVsbG8g"}}
  {"value":"d29ybGQ="}' $T ovdc.KeyValue/Put

# Two entries in one stream -> two PutResponses (index 0 and 1)
grpcurl "${P[@]}" -d '{"begin":{}}
  {"declare":{"key":{"key":"YQ=="},"size":"1","value":"MQ=="}}
  {"declare":{"key":{"key":"Yg=="},"size":"1","value":"Mg=="}}' $T ovdc.KeyValue/Put

# Fixed 32-byte key (u3 = 1 -> 24 zero bytes then 00..01)
grpcurl "${P[@]}" -d '{"keys":[{"fixedKey":{"u0":"0","u1":"0","u2":"0","u3":"1"}}]}' $T ovdc.KeyValue/Get

# Missing key: GET returns no messages; TOUCH returns {} (touched 0)
grpcurl "${P[@]}" -d '{"keys":[{"key":"bm9wZQ=="}]}' $T ovdc.KeyValue/Get

# Tenant isolation: written under team-a, invisible without it
grpcurl "${P[@]}" -H 'tenant: team-a' -d '{"begin":{}} {"declare":{"key":{"key":"ZGVtbw=="},"size":"5","value":"aGVsbG8="}}' $T ovdc.KeyValue/Put
grpcurl "${P[@]}" -H 'tenant: team-b' -d '{"keys":[{"key":"ZGVtbw=="}]}' $T ovdc.KeyValue/Get   # no output unless team-b also wrote it
```

Legacy `/ddcs.KeyValue/*` routing cannot be shown with this proto, because grpcurl builds the path from `package ovdc`. It is covered by the unit test `legacy_package_tests` in `ref/rust/ovdc/src/lib.rs`.

JWT, using the repo's synthetic debug key. Only for a dev server with `grpcJwt.enabled = true` and `jwkPublicKeysetUrl = ["debug"]`:

```bash
JWT=$(sed -n 's/^pub const _DEBUG_JWT_0: &str = "\(.*\)";$/\1/p' rust/ovdc_jwt/src/lib.rs)
grpcurl "${P[@]}" -H "authorization: Bearer $JWT" -d '{"keys":[{"key":"ZGVtbw=="}]}' $T ovdc.KeyValue/Get   # tenant example-tenant-a
```

TLS, using the repo test certs (CN/SAN `localhost`). Server config: `grpcTls.enabled = true`, `cert = "<abs>/rust/tests/test_certs/localhost.crt"`, `key = "<abs>/rust/tests/test_certs/localhost.key"`:

```sh
grpcurl -cacert rust/tests/test_certs/rootCA.crt -import-path rust/ovdc_grpc/proto -proto rpc.proto \
  -d '{"keys":[{"key":"ZGVtbw=="}]}' localhost:3010 ovdc.KeyValue/Touch
```

## Python client (grpcio)

Like the grpcurl examples, run these from `shared-derived-cache/reference/`. Generate stubs from the repo proto:

```sh
python3 -m venv .venv && . .venv/bin/activate
pip install grpcio grpcio-tools
python -m grpc_tools.protoc -I rust/ovdc_grpc/proto \
  --python_out=. --grpc_python_out=. rpc.proto      # -> rpc_pb2.py, rpc_pb2_grpc.py
```

`ovdc_client.py`:

```python
#!/usr/bin/env python3
"""Minimal OVDC KeyValue client: PUT -> GET -> TOUCH round trip."""
import argparse
import sys

import grpc

import rpc_pb2 as pb
import rpc_pb2_grpc as pb_grpc

CHUNK = 1_000_000  # bytes per PutRequest message; keep below grpc.maxDecodingMessageSize


def put(stub, key: bytes, value: bytes, ttl_ms=None, durable=False, metadata=()):
    def requests():
        open_kwargs = {}
        if ttl_ms is not None:
            open_kwargs["ttl"] = ttl_ms
        if durable:
            open_kwargs["durability"] = pb.PutOpen.Normal
        yield pb.PutRequest(begin=pb.PutOpen(**open_kwargs))
        first, rest = value[:CHUNK], value[CHUNK:]
        yield pb.PutRequest(declare=pb.PutDeclare(key=pb.Key(key=key), size=len(value), value=first))
        for i in range(0, len(rest), CHUNK):
            yield pb.PutRequest(value=rest[i:i + CHUNK])

    # One PutResponse per declared entry; this stream declares one entry.
    return list(stub.Put(requests(), metadata=metadata))


def get(stub, keys, metadata=()):
    """Returns {tag: bytes}; missing keys produce no messages."""
    out, sizes = {}, {}
    for msg in stub.Get(pb.GetRequest(keys=[pb.Key(key=k) for k in keys]), metadata=metadata):
        out[msg.tag] = out.get(msg.tag, b"") + msg.value
        sizes[msg.tag] = msg.size
    for tag, size in sizes.items():
        if len(out[tag]) != size:
            raise RuntimeError(f"tag {tag}: got {len(out[tag])} of {size} bytes")
    return out


def touch(stub, keys, metadata=()):
    return stub.Touch(pb.TouchRequest(keys=[pb.Key(key=k) for k in keys]), metadata=metadata)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--target", default="localhost:3010")
    ap.add_argument("--tenant", help="sent as 'tenant' metadata (grpc.tenantFromMetadata)")
    ap.add_argument("--jwt", help="sent as 'authorization: Bearer <jwt>'")
    ap.add_argument("--cacert", help="PEM CA file; enables TLS")
    ap.add_argument("--size", type=int, default=5, help="value size in bytes")
    args = ap.parse_args()

    md = []
    if args.tenant:
        md.append(("tenant", args.tenant))
    if args.jwt:
        md.append(("authorization", f"Bearer {args.jwt}"))

    opts = [("grpc.max_receive_message_length", 64 * 1024 * 1024)]
    if args.cacert:
        creds = grpc.ssl_channel_credentials(open(args.cacert, "rb").read())
        channel = grpc.secure_channel(args.target, creds, options=opts)
    else:
        channel = grpc.insecure_channel(args.target, options=opts)
    stub = pb_grpc.KeyValueStub(channel)

    key = b"demo"
    value = b"hello" if args.size == 5 else bytes(i % 251 for i in range(args.size))

    resp = put(stub, key, value, metadata=md)
    print("PUT  ->", [(pb.PutResponse.PutStatus.Name(r.status), r.index) for r in resp])
    got = get(stub, [key, b"missing-key"], metadata=md)
    print("GET  ->", {t: (len(v), v[:16]) for t, v in got.items()})
    t = touch(stub, [key, b"missing-key"], metadata=md)
    print("TOUCH-> touched", t.touched, "matched", list(t.matched))

    ok = (len(resp) == 1 and resp[0].status == pb.PutResponse.PUT_OK
          and got.get(0) == value and 1 not in got
          and t.touched == 1 and list(t.matched) == [0])
    print("ROUND TRIP", "OK" if ok else "FAILED")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
```

Run it:

```sh
python ovdc_client.py                         # 5-byte value
python ovdc_client.py --size 9000000          # chunked: > blobCutoff, multi-message PUT and GET
python ovdc_client.py --tenant team-a         # tenant-scoped
python ovdc_client.py --cacert rust/tests/test_certs/rootCA.crt   # TLS test certs
```

Expected output:

```
PUT  -> [('PUT_OK', 0)]
GET  -> {0: (5, b'hello')}
TOUCH-> touched 1 matched [0]
ROUND TRIP OK
```

Verification status: the stub generation and this script were run against a Python mock `KeyValue` server built from the same `rpc.proto`, for a 5 B value and a 9 MB value. They were **not** run against the Rust server in this environment. Exit code 0 means the PUT→GET→TOUCH round trip succeeded.

## Confirming the round trip on the server side

- `curl -s localhost:3051/metrics | grep -E 'ovdc_api_request_total|ovdc_api_keys_total'` should show `operation="grpc_put"`, `"grpc_get"`, and `"grpc_touch"` counts rising on the pod you port-forwarded; other pods don't see the request (`ref/rust/ovdc/src/grpc/metrics.rs`). Exact series names are listed in [validate.md](validate.md#metrics-to-watch).
- After the first GC pass, keys touched or read recently survive eviction (`ref/rust/ovdc/src/tests.rs` `test_that_most_recent_key_not_removed`).
