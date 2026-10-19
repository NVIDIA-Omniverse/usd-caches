-- SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
-- SPDX-License-Identifier: Apache-2.0

--[[
Mutation detection for caches running with `requiresVersioning` false.

Prefers an ETag comparison and falls back to Last-Modified when either side has
no ETag. If the upstream validator identifies different content, the cached
entry is purged and the request fails with 503 so the client retries and
repopulates the cache with fresh content. 503 (not 429) because aws-sdk-s3
only retries 5xx transient errors.

Runs in the header_filter phase. On a cache HIT, `$upstream_http_etag` is the
ETag parsed from the cached upstream response. Reading it avoids the response
header timing differences between direct and internal named locations.
Requires `$requires_versioning`, `$upstream_etag`, `$upstream_last_modified`,
and `$mutation_detected` to be set in the location.
]]

local _M = {}

local function purge_stale(http_cache, message)
    http_cache.purge()
    ngx.var.mutation_detected = "1"
    require("lua-metrics").counter.mutation_detection_purges:inc(1, { ngx.var.host })
    ngx.log(ngx.INFO, message)
    return ngx.exit(503)
end

function _M.detect()
    -- ngx.exit() runs the header filter again for the generated 503 response.
    if ngx.var.requires_versioning == "1" or ngx.var.mutation_detected == "1" then return end
    local cache_status = ngx.var.upstream_cache_status or ""
    if cache_status ~= "HIT" then return end

    local upstream_etag = ngx.var.upstream_etag
    local upstream_lm = ngx.var.upstream_last_modified
    local has_upstream_etag = upstream_etag and upstream_etag ~= ""
    local has_upstream_lm = upstream_lm and upstream_lm ~= ""
    if not has_upstream_etag and not has_upstream_lm then return end

    local http_cache = require("http_cache")
    local cached_etag = ngx.var.upstream_http_etag
    if has_upstream_etag and cached_etag and cached_etag ~= "" then
        if cached_etag ~= upstream_etag then
            return purge_stale(http_cache,
                "purged stale cache entry, cached etag: " .. cached_etag ..
                ", upstream etag: " .. upstream_etag)
        end
        return
    end

    local cache_metadata = http_cache.get_metadata()
    if not cache_metadata then return end
    local upstream_time = has_upstream_lm and ngx.parse_http_time(upstream_lm) or nil
    if cache_metadata.last_modified and upstream_time and cache_metadata.last_modified < upstream_time then
        return purge_stale(http_cache,
            "purged stale cache entry, cached last-modified: " ..
            ngx.http_time(cache_metadata.last_modified) ..
            ", upstream last-modified: " .. upstream_lm)
    end
end

return _M
