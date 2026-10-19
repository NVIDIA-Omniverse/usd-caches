-- SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
-- SPDX-License-Identifier: Apache-2.0

--[[
This Lua script implements access control for S3 bucket access through NGINX in a
multi-tenant environment. It handles HEAD, GET, and presigned URL requests,
balancing access control with performance optimization through caching.

Features:
    - Locking mechanism to manage concurrent requests (similar to `proxy_cache_lock`).
    - Authentication caching for HEAD requests.
    - Presigned URL verification and caching.
    - Public vs. authenticated access distinction for GET requests.

Requires the following Lua shared dictionaries to be declared in NGINX conf:
    - lock_by_request_uri_table: Required for locking mechanism.
    - head_request_cache: Stores bursty HEAD requests.
    - presigned_url_cache: Caches verified presigned URLs honoring their expiration.

Requires the following NGINX conf variables:
    - `cache_ttl`: Time to live for the head request cache.
    - `disable_cache`: Set to '1' when the access check couldn't be performed.

Usage:
This script should be included in the NGINX conf using the `access_by_lua_file` directive.

]]

local http = require "resty.http"
local lock = require "lua-lock"
local metrics = require "lua-metrics"

local REQ_KIND_UNKNOWN = -1;
local REQ_KIND_GET = 0;
local REQ_KIND_GET_PRESIGNED_S3 = 1;
local REQ_KIND_GET_PRESIGNED_AZURE = 2;
local REQ_KIND_GET_WITH_AUTH = 3;
local REQ_KIND_HEAD = 4;

local MIN_CACHE_EXPIRATION_THRESHOLD = 10

-- Returns true if content caching is disabled (Cache-Control: no-cache).
local function is_cache_disabled()
    return ngx.var.disable_cache ~= nil and ngx.var.disable_cache == "1"
end

-- Disabling caching forces NGINX to reach out to upstream,
-- therefore access check can be skipped.
local function disable_cache_on_error()
    ngx.var.disable_cache = "1"
end

-- Captures validators returned by an access probe for comparison with the
-- cached response in the header filter phase.
local function capture_upstream_validators(headers)
    if ngx.var.upstream_etag ~= nil then
        local etag = headers["ETag"]
        if etag then
            ngx.var.upstream_etag = etag
        end
    end

    if ngx.var.upstream_last_modified ~= nil then
        local last_modified = headers["Last-Modified"]
        if last_modified then
            ngx.var.upstream_last_modified = last_modified
        end
    end
end


-- Preserves request_uri's error-response behavior while successful access
-- probes stop after the response headers and never buffer the selected body.
local function request_access_uri(httpc, uri, params)
    local parsed_uri, err = httpc:parse_uri(uri, false)
    if not parsed_uri then
        return nil, err
    end

    params.scheme = parsed_uri[1]
    params.host = parsed_uri[2]
    params.port = parsed_uri[3]
    params.path = params.path or parsed_uri[4]
    params.query = params.query or parsed_uri[5]
    params.ssl_server_name = params.ssl_server_name or params.host

    local ok
    ok, err = httpc:connect(params)
    if not ok then
        return nil, err
    end

    local res
    res, err = httpc:request(params)
    if not res then
        httpc:close()
        return nil, err
    end

    if res.status == ngx.HTTP_OK or res.status == ngx.HTTP_PARTIAL_CONTENT then
        local close_ok, close_err = httpc:close()
        if not close_ok then
            ngx.log(ngx.ERR, close_err)
        end
        return res, nil
    end

    local body
    body, err = res:read_body()
    if not body then
        httpc:close()
        return nil, err
    end
    res.body = body

    if params.keepalive == false then
        local close_ok, close_err = httpc:close()
        if not close_ok then
            ngx.log(ngx.ERR, close_err)
        end
    else
        local keepalive_ok, keepalive_err = httpc:set_keepalive(
            params.keepalive_timeout,
            params.keepalive_pool
        )
        if not keepalive_ok then
            ngx.log(ngx.ERR, keepalive_err)
        end
    end

    return res, nil
end


-- Checks access to the request URI. If the resource does not exist or access is forbidden,
-- this function will return an error to the client and NGINX will not handle the request further.
-- Otherwise, this function returns the status code received from upstream, and the caller
-- can decide whether to cache it or do something else.
local function access_upstream_resource()
    local headers = ngx.req.get_headers()
    -- The request retrieves only the first byte (there are no empty ranges).
    -- This is enough to verify resource accessibility.
    -- Main NGINX processing will handle the full body download if needed.
    headers["Range"] = "bytes=0-0"

    local function do_request()
        local httpc = http:new()
        local uri = ngx.var.proxy_auth_pass
        if not uri or uri == "" then
            uri = ngx.var.scheme .. "://" .. ngx.var.host .. ngx.var.request_uri
        else
            uri = uri .. ngx.var.request_uri
        end

        metrics.counter.access_request:inc(1, { ngx.var.host, ngx.var.request_method })
        local timer = metrics.timer(metrics.histogram.access_upstream_request_time)

        local response, err = request_access_uri(
            httpc,
            uri,
            {
                method = ngx.var.request_method,
                headers = headers
            }
        )

        timer({ ngx.var.host })

        if response then
            metrics.counter.access_response:inc(1, { ngx.var.host, ngx.var.request_method, response.status})
        end

        return response, err
    end


    local res, err = do_request()
    if not res then
        ngx.log(ngx.WARN, "⚠️ retrying auth request: ", err)
        res, err = do_request()
    end

    if not res then
        ngx.log(ngx.WARN, "⚠️ auth request failed: ", err)
        disable_cache_on_error()
        return ngx.HTTP_INTERNAL_SERVER_ERROR
    end

    capture_upstream_validators(res.headers)

    if res.status == ngx.HTTP_PARTIAL_CONTENT then
        -- Treat partial content as OK since it indicates the resource exists
        -- and is accessible.
        return ngx.HTTP_OK
    elseif res.status == ngx.HTTP_FORBIDDEN then
        -- AWS doesn't return a body for Forbidden errors, so we can return
        -- the response immediately and close the connection.
        ngx.status = res.status
        ngx.header = res.headers
        ngx.log(ngx.WARN, "⚠️ request forbidden")
        return ngx.exit(res.status)
    else
        return res.status
    end
end

local function head_upstream_resource()
    local httpc = http:new()
    local uri = ngx.var.proxy_auth_pass
    if not uri or uri == "" then
        uri = ngx.var.scheme .. "://" .. ngx.var.host .. ngx.var.request_uri
    else
        uri = uri .. ngx.var.request_uri
    end

    metrics.counter.access_request:inc(1, { ngx.var.host, "HEAD" })
    local timer = metrics.timer(metrics.histogram.access_upstream_request_time)

    -- Content freshness is separate from the client's response conditions.
    -- Fetch unconditional validators so a conditional request cannot hide a
    -- mutation that must purge the cached entry.
    local headers = ngx.req.get_headers()
    headers["if-match"] = nil
    headers["if-modified-since"] = nil
    headers["if-none-match"] = nil
    headers["if-unmodified-since"] = nil
    local res, err = httpc:request_uri(uri, { method = "HEAD", headers = headers })

    timer({ ngx.var.host })

    if not res then
        ngx.log(ngx.WARN, "HEAD upstream failed: ", err)
        return
    end

    metrics.counter.access_response:inc(1, { ngx.var.host, "HEAD", res.status })

    capture_upstream_validators(res.headers)
end

local function parse_iso8601_date(date_string, allow_minute_precision)
    if type(date_string) ~= "string" then
        return nil
    end

    local year, month, day, hour, min, sec =
        date_string:match("^(%d%d%d%d)-?(%d%d)-?(%d%d)T(%d%d):?(%d%d):?(%d%d)Z$")
    if not year and allow_minute_precision then
        -- Azure also accepts minute-precision SAS times such as
        -- 2026-11-28T17:25Z. Treat omitted seconds as zero.
        year, month, day, hour, min =
            date_string:match("^(%d%d%d%d)-?(%d%d)-?(%d%d)T(%d%d):?(%d%d)Z$")
        sec = year and "00" or nil
    end
    if not (year and month and day and hour and min and sec) then
        return nil
    end
    return os.time({ year = year, month = month, day = day, hour = hour, min = min, sec = sec })
end

-- Checks that both dates are set and that the expiration date is after the creation date.
local function is_valid_date_range(created_date, expires_date)
    return created_date and expires_date and expires_date > created_date
end

-- Determines whether the current request is for a presigned URL. If so,
-- it calculates the expiration time. Otherwise, it returns nil.
local function try_get_time_s3(url_args)
    local x_aws_date, x_aws_expires, x_aws_signature
    for k, v in pairs(url_args) do
        if k == "X-Amz-Date" then
            x_aws_date = v
        elseif k == "X-Amz-Expires" then
            x_aws_expires = v
        elseif k == "X-Amz-Signature" then
            -- The signature value is not used, but its presence indicates
            -- this is a presigned URL.
            x_aws_signature = v
        end
        if x_aws_date and x_aws_expires and x_aws_signature then break end
    end
    if x_aws_date and x_aws_expires and x_aws_signature then
        -- Parse ISO-8601 date.
        local aws_date = parse_iso8601_date(x_aws_date)
        if not aws_date then return nil, nil end

        local expires = tonumber(x_aws_expires)
        if not expires then return nil, nil end

        local expiration_time = aws_date + tonumber(expires)
        return aws_date, expiration_time
    end
    -- Not S3 presigned or missing required parameters.
    return nil, nil
end

-- The URL has been identified as: Azure presigned
-- Get the creation and expiration times from the URL args.
-- Note: The 'st' (start time) parameter is optional in Azure SAS tokens.
-- When absent, the start time is implicitly "now" (current time).
local function try_get_time_azure(uri_args)
    local created_string = uri_args["st"]
    local expiration_string = uri_args["se"]

    -- Expiration (se) is required for presigned URLs.
    if not expiration_string then return nil, nil end

    -- Start time (st) is optional; an absent or empty value defaults to now,
    -- while a supplied malformed value must remain invalid.
    local created_date
    if created_string == nil or created_string == "" then
        created_date = os.time()
    else
        created_date = parse_iso8601_date(created_string, true)
    end
    local expiration_date = parse_iso8601_date(expiration_string, true)

    return created_date, expiration_date
end

local function identify_request(http_method, request_headers, request_uri_args)
    if http_method == "HEAD" then
        return REQ_KIND_HEAD
    elseif http_method ~= "GET" then
        return REQ_KIND_UNKNOWN
    end

    -- Presigned URLs take precedence over Authorization header. A presigned URL
    -- contains its own signature in the query params, so Authorization header
    -- is irrelevant if present.
    if request_uri_args["X-Amz-Signature"] then
       return REQ_KIND_GET_PRESIGNED_S3
    elseif request_uri_args["sig"] then
       return REQ_KIND_GET_PRESIGNED_AZURE
    end

    -- Check for Authorization header (authenticated but not presigned).
    if request_headers["Authorization"] then
        return REQ_KIND_GET_WITH_AUTH
    end

    return REQ_KIND_GET
end

local function check_verified_presigned_url(presigned_url_cache, presigned_url)
    if not presigned_url_cache then return nil end
    local cached_status = presigned_url_cache:get(presigned_url)
    return cached_status
end

-- Caches a verified presigned URL if it's not expiring soon.
local function cache_verified_presigned_url(presigned_url_cache, presigned_url, expiration)
    if not presigned_url_cache then return end
    local current_time = os.time()
    local time_until_expiration_in_seconds = expiration - current_time
    if time_until_expiration_in_seconds >= MIN_CACHE_EXPIRATION_THRESHOLD then
        assert(MIN_CACHE_EXPIRATION_THRESHOLD > 2)
        local cache_ttl = time_until_expiration_in_seconds - 2
        presigned_url_cache:set(presigned_url, true, cache_ttl)
    end
end

local function do_presigned_url_miss_under_lock(presigned_url_cache, presigned_url, expiration)
    -- Checks the cache again under lock. It may be already populated.
    if check_verified_presigned_url(presigned_url_cache, presigned_url) then
        return
    end
    local status = access_upstream_resource()
    if status == ngx.HTTP_OK then
        cache_verified_presigned_url(presigned_url_cache, presigned_url, expiration)
    end
end

local function handle_presigned_url_miss(presigned_url_cache, presigned_url, expiration)
    local result, err = lock.with_lock(function()
        do_presigned_url_miss_under_lock(presigned_url_cache, presigned_url, expiration)
    end, ngx.var.request_uri, 5)
    if not result and err then
        disable_cache_on_error()
        return ngx.HTTP_INTERNAL_SERVER_ERROR
    end
end

local function handle_presigned_url_request(presigned_url, expiration)
    local presigned_url_cache = ngx.shared.presigned_url_cache

    if check_verified_presigned_url(presigned_url_cache, presigned_url) then
        -- Let NGINX proceed, the presigned URL has not expired.
        return
    end

    local err = handle_presigned_url_miss(presigned_url_cache, presigned_url, expiration)
    if err then
        return err
    end
end

local function handle_request()
    -- Bail out as Access Checks are only relevant when content is cached.
    if is_cache_disabled() then return false end

    --  Determines the request type and handles it accordingly.
    local request_uri_args = ngx.req.get_uri_args()
    local request_kind = identify_request(ngx.req.get_method(), ngx.req.get_headers(), request_uri_args)

    if request_kind == REQ_KIND_GET_PRESIGNED_S3 then
        -- S3 Presigned
        local created, expiration = try_get_time_s3(request_uri_args)
        if is_valid_date_range(created, expiration) then
            handle_presigned_url_request(ngx.var.request_uri, expiration)
            return
        end
        ngx.log(ngx.WARN,
            "⚠️ created time or expiration is invalid for s3 request, ",
            "created: ", tostring(created), ", ", "expiration: ", tostring(expiration))
        return ngx.exit(ngx.HTTP_BAD_REQUEST)

    elseif request_kind == REQ_KIND_HEAD then
        -- Head requests are not cached in NGINX core. Threfore there is no need to perform
        -- an access check. See "cache-bypass.conf" for more details.
        -- IE do nothing

    elseif request_kind == REQ_KIND_GET then
        if ngx.var.requires_versioning == "0" then
            head_upstream_resource()
        end

    elseif request_kind == REQ_KIND_GET_WITH_AUTH then
        -- For URLs with an 'Authorization' header, the resource must have its access
        -- checked prior to serving a potentially cached response.
        access_upstream_resource()

    elseif request_kind == REQ_KIND_GET_PRESIGNED_AZURE then
        -- Azure Presigned (SAS)
        local created, expiration = try_get_time_azure(ngx.req.get_uri_args())
        if not is_valid_date_range(created, expiration) then
            ngx.log(ngx.WARN,
                "⚠️ created time or expiration is invalid for azure request, ",
                "created: ", tostring(created), ", ", "expiration: ", tostring(expiration))
            return ngx.exit(ngx.HTTP_BAD_REQUEST)
        end
        -- NOTE: When asked for If-Match: XYZ Azure will respond with ETag: "XYZ"
        -- This causes a problem when NGINX reconciles the HTTP conditions
        -- If this behavior is required the following fixes it:
        -- ngx.req.clear_header("if-match")
        handle_presigned_url_request(ngx.var.request_uri, expiration)

    end

    return true
end

-- Main execution.
if _G._UNIT_TEST == true then
    -- Provide testable functions.
    return {
        handle_request = handle_request,
        is_cache_disabled = is_cache_disabled,
        parse_iso8601_date = parse_iso8601_date,
        identify_request = identify_request,
        try_get_time_azure = try_get_time_azure,
        try_get_time_s3 = try_get_time_s3,
        is_valid_date_range = is_valid_date_range,
        access_upstream_resource = access_upstream_resource,
        head_upstream_resource = head_upstream_resource,

        REQ_KIND_UNKNOWN = REQ_KIND_UNKNOWN,
        REQ_KIND_GET = REQ_KIND_GET,
        REQ_KIND_GET_PRESIGNED_S3 = REQ_KIND_GET_PRESIGNED_S3,
        REQ_KIND_GET_PRESIGNED_AZURE = REQ_KIND_GET_PRESIGNED_AZURE,
        REQ_KIND_GET_WITH_AUTH = REQ_KIND_GET_WITH_AUTH,
        REQ_KIND_HEAD = REQ_KIND_HEAD,
    }
else
    -- Execute immediately, as this module is not loaded within
    -- a unit testing context.
    local timer = metrics.timer(metrics.histogram.access_time)
    handle_request()
    timer({ngx.var.host})
end
