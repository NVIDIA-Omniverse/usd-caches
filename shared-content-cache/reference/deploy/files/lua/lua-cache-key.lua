-- SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
-- SPDX-License-Identifier: Apache-2.0

--[[
Computes the Azure Get Blob cache-key variant for `azure-server.conf`. It stays
a pure module so busted can pin the component NGINX feeds into
`proxy_cache_key` through `$azure_cache_variant`.
]]

local _M = {}

local function header_value(headers, name)
    local value = headers and headers[name]
    if type(value) == "string" then
        return value
    end
    return ""
end

local function length_prefixed(name, value)
    return name .. "=" .. #value .. ":" .. value
end

-- Extends Azure's effective range with request fields that select a distinct
-- Get Blob representation or checksum response. A plain range keeps its legacy
-- key component so equivalent `Range` and `x-ms-range` requests still share.
function _M.azure_get_variant(headers, effective_range)
    local fields = {
        { "structured", header_value(headers, "x-ms-structured-body") },
        { "md5", header_value(headers, "x-ms-range-get-content-md5") },
        { "crc64", header_value(headers, "x-ms-range-get-content-crc64") },
        { "version", header_value(headers, "x-ms-version") },
        { "upn", header_value(headers, "x-ms-upn") },
        { "origin", header_value(headers, "origin") },
    }
    local has_variant = false
    for _, field in ipairs(fields) do
        has_variant = has_variant or field[2] ~= ""
    end
    if not has_variant then
        return effective_range or ""
    end

    local range = effective_range or ""
    local parts = { length_prefixed("range", range) }
    for _, field in ipairs(fields) do
        parts[#parts + 1] = length_prefixed(field[1], field[2])
    end
    return table.concat(parts, ";")
end

return _M
