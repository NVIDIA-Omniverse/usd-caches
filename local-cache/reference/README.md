# Omniverse Hub

NVIDIA OmniHub is the new generation Client Library service. It provides efficient communication between Omniverse
Applications and local or server data storage, such as Nucleus or S3. OmniHub also functions as a local file cache
for transferred data, improving load times for frequently accessed files.

## Developing

Building / Unittests / Formatting / Static Analysis (clippy)

```shell
# build debug
$ cargo build

# build release
$ cargo build --release

# test debug
$ cargo test

# test release
$ cargo test --release

# format
$ cargo fmt

# clippy (static analyzer)
$ cargo clippy

# clippy (static analyzer) release
$ cargo clippy --release
```

## Running Local Omniverse Hub

```shell
# debug hub
$ cargo run
# release hub
$ cargo run --release
```

or

```shell
# debug hub
$ cargo run --bin hub
# release hub
$ cargo run --release --bin hub
```

## Notes

If you're using a Linux flavour, you need to run `sudo apt install libssl-dev` and `sudo apt install pkg-config` to avoid `openssl` errors.

## Hub Configurations

Hub follows Nvidia standard configurations. To override any of the configurations please create/edit the file located
at `~/.nvidia-omniverse/config/hub.toml`.

### CLI Help:

```bash
cargo run --release --bin hub -- --help
```

Config example:

```toml
[cache]
path = "cache/path"
max_size = "40g"
cleanup_threshold = 10
garbage_collection_interval = 300

[http]
host = "127.0.0.1"
port = 14090

[logging]
path = "log/path"
[logging.file]
enabled = true
color = true
max_level = "trace"
file = "hub.log"
[logging.stdout]
enabled = true
color = true
max_level = "trace"
[logging.stderr]
enabled = true
color = true
max_level = "trace"

[runtime]
num_worker_threads = 2

[transport.tcp]
host = "127.0.0.1"
rpc_port = 14092
ext_port = 14093
```

## API

Please check the existing endpoints in the file `hub/api.http`.

## Protobuf RPC Interface

Hub exposes a Protobuf RPC interface on port 14093 (by default). The Protobuf RPC interface provides the same functionality as the MessagePack RPC interface, allowing developers to integrate with Hub using standard Protobuf tooling. This is a standard TCP-based protocol with framed messages, using a 4-byte length specifier. The expected workflow for a client is to open a single connection and to send all requests related to the client's working set across that connection, as Hub maintains state on a per-connection basis.

### Protobuf Definitions

The protocol buffer definitions are located at `hub-rpc/proto/nvidia/omniverse/usd_caches/local_cache/v1beta/local_cache.proto`.

### Protobuf RPC Examples

Sample code is available in `hub-rpc/examples`.

### Port Configuration

The Protobuf port can be configured in `hub.toml`, and is represented by the `ext_port` field under `[transport.tcp]`. The `rpc_port` field is used by the legacy messagepack protocol.

```toml
[transport.tcp]
host = "127.0.0.1"
rpc_port = 14092  # MessagePack RPC port
ext_port = 14093  # Protobuf RPC port
```
