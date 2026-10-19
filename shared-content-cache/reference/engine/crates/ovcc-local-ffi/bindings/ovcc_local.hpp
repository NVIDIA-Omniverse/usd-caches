// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
#pragma once
#include "ovcc_local.h"
#include <filesystem>
#include <future>
#include <memory>
#include <optional>
#include <stdexcept>
#include <string>
#include <utility>
#include <vector>

namespace ovcc {
using headers = std::vector<std::pair<std::string, std::string>>;

struct request {
    std::string authority, path, query;
    headers request_headers;
    bool nucleus = false;
    uint64_t lease_duration_millis = 30000;
};

struct response {
    ovcc_local_outcome outcome;
    uint16_t status;
    uint64_t generation, offset, length, deadline_unix_millis;
    std::optional<std::filesystem::path> path;
    std::optional<std::string> cache_status;
    headers response_headers;
};

class error : public std::runtime_error {
public:
    int32_t code;
    explicit error(const ovcc_local_error &value)
        : std::runtime_error(value.message), code(value.code) {}
};

// Copies share a bounded pool. Async calls retain the client themselves.
class client {
    std::shared_ptr<ovcc_local_client> native_;
    static const char *text(const std::string &value) {
        if (value.find('\0') != std::string::npos)
            throw std::invalid_argument("request strings cannot contain NUL");
        return value.c_str();
    }
public:
    explicit client(uint64_t timeout_ms = 30000, size_t max_connections = 4) {
        if (ovcc_local_abi_version() != 1)
            throw std::runtime_error("incompatible OVCC native bridge");
        ovcc_local_client *raw = nullptr;
        ovcc_local_error failure{};
        if (ovcc_local_client_new_with_pool_v1(timeout_ms, max_connections, &raw, &failure)) throw error(failure);
        native_ = {raw, ovcc_local_client_free_v1};
    }

    response get(const request &input) const {
        if (!native_) throw std::logic_error("client was moved from");
        std::vector<ovcc_local_header> fields;
        for (const auto &field : input.request_headers)
            fields.push_back({text(field.first), text(field.second)});
        ovcc_local_request wire{text(input.authority), text(input.path), text(input.query),
            fields.data(), fields.size(), static_cast<uint8_t>(input.nucleus),
            input.lease_duration_millis};
        ovcc_local_response *raw = nullptr;
        ovcc_local_error failure{};
        if (ovcc_local_get_v1(native_.get(), &wire, &raw, &failure)) throw error(failure);
        std::unique_ptr<ovcc_local_response, decltype(&ovcc_local_response_free_v1)>
            owner(raw, ovcc_local_response_free_v1);
        ovcc_local_response_view view{};
        ovcc_local_response_view_v1(owner.get(), &view);
        response result{static_cast<ovcc_local_outcome>(view.outcome), view.status,
            view.generation, view.offset, view.length, view.deadline_unix_millis,
            std::nullopt, std::nullopt, {}};
        if (view.path) result.path = std::filesystem::u8path(view.path);
        if (view.cache_status) result.cache_status = view.cache_status;
        for (size_t i = 0; i < view.header_count; ++i)
            result.response_headers.emplace_back(view.headers[i].name, view.headers[i].value);
        return result;
    }

    std::future<response> get_async(request input) const {
        return std::async(std::launch::async, [self = *this, input = std::move(input)] {
            return self.get(input);
        });
    }
};
} // namespace ovcc
