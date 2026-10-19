<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Local path clients

These experimental Rust, Python and C++ clients return an immutable local path,
byte range, response metadata and an open-by UTC deadline. They do not open or
map files. Applications choose their own I/O, respect the range, open read-only
before the deadline and decide whether to request again after an open failure.
There is no finish, acknowledgment, renewal task or lease-release operation.

OVCC must run on the same OS under the same user and pathname namespace with
`service.localMode: true`. Windows and WSL2 are separate namespaces.
Only persisted unexpired deadlines survive an OVCC restart. An opened file or
application-owned mapping follows OS lifetime rules and can delay disk reclamation.

The APIs and C bridge are provisional. Build and distribute the library, header
and wrappers together; layout version 1 detects mismatches, not a public ABI
stability promise. The separately frozen binary wire protocol remains v1.
Nothing is published to a package registry.

## Rust

Depend on `ovcc-local` by path or through the internal registry when published.
`SharedClient` is cloneable and owns a private pool, with a default cap of four
connections. Use `SharedClient::new(max_connections)?` to choose another cap.

```rust
let client = ovcc_local::SharedClient::default();
let mut request = ovcc_local::LeaseRequest::new("objects.example", "/foo.usd");
request.headers.push(("If-Match".into(), "\"v1\"".into()));
match client.lease(&request).await? {
    ovcc_local::LeaseResult::Ready(file) => {
        let metadata = file.into_response();
        // The application chooses how to use metadata.path, offset and length.
    }
    ovcc_local::LeaseResult::Unavailable(response) => {
        // The application handles the outcome and ordinary response status.
    }
}
```

The call is safe Rust; the client performs no mmap or object-file access.
Cancellation or malformed exchanges discard the affected connection and return
capacity; no request is automatically replayed. The caller can wrap the future
in a timeout covering queueing, connection establishment and the exchange.
`Client::connect_local()` remains the lower-level dedicated-connection API.
Run `cargo run -p ovcc-local --example path -- objects.example /foo.usd 'If-Match="v1"'`
from `engine/` for a metadata-only example.

## Python and C++

From `engine/`, build `cargo build --locked -p ovcc-local-ffi`.
Linux produces `target/debug/libovcc_local_ffi.so`; Windows produces
`target/debug/ovcc_local_ffi.dll` and its `.dll.lib` import library.

Python 3.10+ uses the standard-library ctypes wrapper. Add this directory to
`PYTHONPATH` and pass the library path explicitly:

```python
from ovcc_local import Client, Outcome, Request

client = Client("/absolute/path/libovcc_local_ffi.so", max_connections=4)
result = await client.get_async(Request(
    "objects.example", "/foo.usd", headers=(("If-Match", '"v1"'),)))
if result.outcome == Outcome.READY:
    print(result.path, result.offset, result.length, result.deadline_unix_millis)
```

`client.get(request)` is the blocking equivalent. Each call includes queueing,
connection establishment and the full exchange in its timeout, defaulting to
30 seconds. Calls share the Rust pool, capped at four connections by default.
An idle connection is reused before opening another, and calls wait when the
pool is full. Interrupted exchanges are discarded; later calls use a healthy
connection or reconnect, without replaying a failed call.
Python async cancellation stops waiting while the
timeout-bounded native worker retains the client and completes.

C++17 includes `ovcc_local.hpp` and links the native library:

```cpp
ovcc::client client(30000, 4); // Timeout in milliseconds, then connection cap.
ovcc::request request;
request.authority = "objects.example";
request.path = "/foo.usd";
request.request_headers.emplace_back("If-Match", "\"v1\"");
auto result = client.get_async(request).get(); // get(request) is blocking.
```

Client copies share the pool; async calls retain their own copy. C++ async calls
use `std::async`, so destroying the last future can wait for completion.
Responses contain copied values independent of client lifetime. Non-ready
responses preserve their outcome, status and headers instead of throwing;
transport failures throw `ovcc::error` or Python `Error`.

For Linux, compile the example with
`c++ -std=c++17 -pthread -Ibindings bindings/examples/path.cpp -L../../target/debug -lovcc_local_ffi -o path`
from this crate's directory and set `LD_LIBRARY_PATH` to the library directory.
On Windows, use an x64 MSVC developer shell:
`cl /EHsc /std:c++17 /Ibindings bindings\examples\path.cpp ..\..\target\debug\ovcc_local_ffi.dll.lib`.
Place the DLL beside the executable or on `PATH`.
The Python example takes the library, authority and path followed by
`--header name=value`; the C++ example takes authority and path followed by
`name=value`. Avoid command-line credentials in shared environments.

## Validation

From the repository root, build OVCC, the bridge, the Rust example and the mock
server, then run `bindings/tests/run.py` with explicit artifact paths:

```sh
cargo build --locked --manifest-path engine/Cargo.toml -p ovcc -p ovcc-local-ffi
cargo build --locked --manifest-path engine/Cargo.toml -p ovcc-local --example path
cargo build --locked --manifest-path tests/mock-server/Cargo.toml
python engine/crates/ovcc-local-ffi/bindings/tests/run.py \
  --library engine/target/debug/libovcc_local_ffi.so \
  --ovcc-bin engine/target/debug/ovcc \
  --mock-bin tests/mock-server/target/debug/mock-server \
  --rust-example engine/target/debug/examples/path --output /path/to/evidence
```

Use the corresponding DLL and EXE paths on Windows. The runner discovers the
installed MSVC x64 tools, compiles both C++ programs, runs Python/C++ tests
against real OVCC and the mock server, and executes all three examples.
It requires the same-user local endpoint to be free. It uses an isolated cache,
including a non-ASCII directory name, and stops only processes it started.
Tests cover MISS/HIT, empty files, unavailable replies, concurrent calls at
caps 1/2/4/8, timeout/reconnect, copied-value lifetime and application-owned mapping.
This is not a downstream integration test or a performance measurement.
