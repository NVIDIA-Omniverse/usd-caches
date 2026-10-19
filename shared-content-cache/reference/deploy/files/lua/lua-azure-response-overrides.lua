-- SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
-- SPDX-License-Identifier: Apache-2.0

--[[
Replays Azure response-header overrides after a normalized cache lookup.

Azure response overrides are deliberately absent from the object cache key, so
reads that differ only in requested response headers share one identity. A
cached response does not reach Azure, so the current request supplies its
response metadata.
]]

local _M = {}

local OVERRIDES = {
    rscc = "Cache-Control",
    rscd = "Content-Disposition",
    rsce = "Content-Encoding",
    rscl = "Content-Language",
    rsct = "Content-Type",
}

local function safe_value(value)
    return type(value) == "string" and not value:find("[\r\n]")
end

-- Returns the supported, safe Azure response overrides from decoded URI args.
-- Multi-valued and CR/LF-containing fields are ignored so an untrusted query
-- cannot alter the response header structure.
function _M.headers(args)
    local headers = {}
    for arg, header in pairs(OVERRIDES) do
        local value = args and args[arg]
        if safe_value(value) then
            headers[header] = value
        end
    end
    return headers
end

-- Returns Azure's valid client-generated request id for response echoing.
function _M.client_request_id(headers)
    local value = headers and headers["x-ms-client-request-id"]
    if type(value) ~= "string" or #value > 1024 then
        return nil
    end
    for i = 1, #value do
        local byte = value:byte(i)
        if byte < 32 or byte > 126 then
            return nil
        end
    end
    return value
end

-- Applies supported request overrides to the current response. It runs in the
-- Azure header-filter path after mutation detection, for cache hits and misses.
function _M.apply()
    for header, value in pairs(_M.headers(ngx.req.get_uri_args())) do
        ngx.header[header] = value
    end
    if ngx.var.upstream_cache_status == "HIT" then
        -- Azure's request ID identifies an origin operation and must not be
        -- replayed by a later cache hit. The cached origin Date remains the
        -- representation's HTTP freshness timestamp; Age describes residency.
        -- https://learn.microsoft.com/en-us/rest/api/storageservices/get-blob#response-headers
        ngx.header["x-ms-request-id"] = nil
    end
    ngx.header["x-ms-client-request-id"] = _M.client_request_id(ngx.req.get_headers())
end

return _M
