# Protobuf RPC

## Rationale

Hub is designed to optimize on-device caching for USD-based workflows. Data is represented by path names for direct file access, with ownership tracked on a per-client basis to maintain data integrity and facilitate garbage collection. 

## Protocol Structure

The RPC protocol uses Protobuf for message representation. Each message is framed using a 4-byte length prefix, and communication occurs via a persistent TCP connection. The expected workflow for a client is to open a single connection and to send all requests related to the client's working set across that connection, as Hub maintains state on a per-connection basis.

There are three basic categories of Hub transaction: service-level transactions, cache-aside transactions, and block-cache transactions. Service-level transactions manage client state and interrogate the service as a whole, while cache-aside transactions and block-cache transactions operate on files and blocks of data, respectively.

Upon connection, a client is expected to identify itself via the service-level ClientConnect request, after which both cache-aside and block-cache operations may be performed. In cases where a client is resuming after a shutdown, state may be reclaimed for connected files and blocks via the CacheAsideConnect and BlockCacheConnect requests. For information on specific transaction types and message structure, see the included `.proto` file or accompanying documentation located in the Hub package's `rpc` directory.

### Protobuf Definitions

The Protobuf definitions are located at `rpc/proto/nvidia/omniverse/usd_caches/local_cache/v1beta/local_cache.proto` within the Hub package.

### Protobuf RPC Examples

Sample code is available in `rpc/examples`.

## Execution

Hub can be run in two modes: shared mode or exclusive mode. In shared mode, Hub will start on pre-configured ports and is expected to service requests from any number of client applications, while in exclusive mode, Hub will start on a random port as a child of the invoking parent process. Typically, when starting in exclusive mode, the `--write-config <filename>` startup option is supplied so that the client can obtain mapped ports for communicating with Hub.

### Port Configuration

The Protobuf port can be configured in `hub.toml`, and is represented by the `ext_port` field under `[transport.tcp]`. The `rpc_port` field is used by the legacy MessagePack protocol, which is functionally identical to the Protobuf protocol.

```toml
[transport.tcp]
host = "127.0.0.1"
rpc_port = 14092  # MessagePack RPC port
ext_port = 14093  # Protobuf RPC port
```
