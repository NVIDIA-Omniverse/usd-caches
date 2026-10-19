// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
#pragma once
#include <stddef.h>
#include <stdint.h>

// Experimental bridge: ships in lockstep with its library and wrappers.
// Strings are UTF-8. Calls may share a client; destruction requires no active calls.
#if defined(_WIN32)
#define OVCC_LOCAL_API __declspec(dllimport)
#else
#define OVCC_LOCAL_API
#endif
#ifdef __cplusplus
extern "C" {
#endif
typedef struct ovcc_local_client ovcc_local_client;
typedef struct ovcc_local_response ovcc_local_response;
typedef struct {
    const char *name;
    const char *value;
} ovcc_local_header;
typedef struct {
    const char *authority;
    const char *path;
    const char *query;
    const ovcc_local_header *headers;
    size_t header_count;
    uint8_t nucleus;
    uint64_t lease_duration_millis;
} ovcc_local_request;
typedef struct {
    int32_t code;
    char message[256];
} ovcc_local_error;
enum ovcc_local_outcome {
    OVCC_LOCAL_READY = 0, OVCC_LOCAL_NOT_LEASABLE = 1,
    OVCC_LOCAL_UNSUPPORTED_LAYOUT = 2, OVCC_LOCAL_REJECTED = 3,
    OVCC_LOCAL_EXPIRED = 4
};
// Pointers borrow the response; copies remain usable after freeing it or the client.
typedef struct {
    uint32_t outcome;
    uint16_t status;
    uint64_t generation;
    uint64_t offset;
    uint64_t length;
    uint64_t deadline_unix_millis;
    const char *path;
    const char *cache_status;
    const ovcc_local_header *headers;
    size_t header_count;
} ovcc_local_response_view;

OVCC_LOCAL_API uint32_t ovcc_local_abi_version(void);
OVCC_LOCAL_API int32_t ovcc_local_client_new_v1(uint64_t timeout_ms,
    ovcc_local_client **output, ovcc_local_error *error);
OVCC_LOCAL_API int32_t ovcc_local_client_new_with_pool_v1(uint64_t timeout_ms,
    size_t max_connections, ovcc_local_client **output, ovcc_local_error *error);
OVCC_LOCAL_API int32_t ovcc_local_get_v1(const ovcc_local_client *client,
    const ovcc_local_request *request, ovcc_local_response **output, ovcc_local_error *error);
OVCC_LOCAL_API void ovcc_local_client_free_v1(ovcc_local_client *client);
OVCC_LOCAL_API void ovcc_local_response_view_v1(const ovcc_local_response *response,
    ovcc_local_response_view *output);
// Frees metadata, not a server-side lease. No finish/release call exists.
OVCC_LOCAL_API void ovcc_local_response_free_v1(ovcc_local_response *response);
#ifdef __cplusplus
}
#endif
