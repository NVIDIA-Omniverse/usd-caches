-- SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
-- SPDX-License-Identifier: Apache-2.0

--[[
This Lua module implements DNS-based upstream balancing with connection pooling
for NGINX. It resolves hostnames to IP addresses, caches DNS results, and sets
up the balancer context for connection reuse.

Features:
    - DNS resolution with configurable timeouts and retries.
    - DNS result caching with TTL management.
    - Round-robin IP selection from DNS records.
    - Fallback nameserver support.

Requires:
    - Shared dictionary `upstreamBalancerCache` for config and DNS cache.
    - NGINX context `ngx.ctx.upstream_peer` to store resolved peer info.

Usage:
    set $upstream_target "$scheme://$host";

    rewrite_by_lua_block {
        local balancer = require "lua-balancer"
        local peer = balancer.resolve_for_keepalive(ngx.var.upstream_target)
        if peer then
            ngx.ctx.upstream_peer = peer
        end
    }
]]

local resolver = require "resty.dns.resolver"
local cjson = require "cjson"

-- Module-level variables.
local cached_config = nil
local dns_rr_rand = 0

-- Indicates DNS lookup failed (cached negative result).
local DNS_FAILURE_MARKER = "(x__x)"

-- Reads nameservers from a resolv.conf file.
-- Returns nameservers list on success, or (default, error) on failure.
local function read_nameservers(filepath, default)
    local nameservers = {}

    local file, err = io.open(filepath, "r")
    if not file then
        return default, "Could not open " .. filepath .. ": " .. (err or "")
    end

    local content = file:read("*a")
    file:close()

    for ns in content:gmatch("nameserver%s+([%x%.:]+)") do
        table.insert(nameservers, ns)
    end

    return nameservers
end

-- Gets the balancer configuration from shared dictionary.
-- Returns config table on success, or (nil, error) on failure.
-- The configuration is set in `init_worker_by_lua_block` in "nginx.conf".
local function get_balancer_config(cache)
    if cached_config then return cached_config end
    local cfg_json = cache:get("config")
    if not cfg_json then return nil, "upstream config not found in shared dict" end
    local config = cjson.decode(cfg_json)
    cached_config = config
    return cached_config
end

-- Resolves bridge DNS hostname to IP address using system DNS.
-- Caches the resolved IP for 60 seconds to avoid repeated DNS queries.
-- Returns IP address on success, or original hostname on failure.
local function resolve_bridge_dns(bridge_dns_hostname, balancer_config, cache)
    local cache_key = "bridge_dns_ip:" .. bridge_dns_hostname
    local ttl = 60

    -- Checks cache first.
    local cached_ip = cache:get(cache_key)
    if cached_ip then
        ngx.log(ngx.DEBUG, "using cached bridge DNS IP: ", cached_ip)
        return cached_ip
    end

    -- Bootstraps using system DNS.
    local system_nameservers, ns_err = read_nameservers("/etc/resolv.conf", balancer_config.resolver.fallback)
    if ns_err then
        ngx.log(ngx.WARN, "⚠️ cannot read resolv.conf for bridge resolver bootstrap: ", ns_err)
        return bridge_dns_hostname
    end

    local bootstrap_resolver, resolve_err = resolver:new{
        nameservers = system_nameservers,
        timeout = balancer_config.resolver.query.timeout_ms,
        retrans = balancer_config.resolver.query.retries
    }

    if not bootstrap_resolver then
        ngx.log(ngx.WARN, "⚠️ failed to create bootstrap resolver: ", resolve_err)
        return bridge_dns_hostname
    end

    local answers, bootstrap_resolve_err = bootstrap_resolver:query(bridge_dns_hostname, {qtype = bootstrap_resolver.TYPE_A})
    if answers and not answers.errcode and answers[1] and answers[1].address then
        local resolved_ip = answers[1].address
        ngx.log(ngx.INFO, "✅ resolved bridge DNS ", bridge_dns_hostname, " to ", resolved_ip)
        -- Caches the resolved IP.
        cache:set(cache_key, resolved_ip, ttl)
        return resolved_ip
    else
        local err_msg = bootstrap_resolve_err or (answers and answers.errstr) or "no A records"
        ngx.log(ngx.WARN, "⚠️ failed to resolve bridge DNS hostname, using as-is: ", err_msg)
        return bridge_dns_hostname
    end
end

-- Splits a "host:port" string into hostname and numeric port.
-- Returns (hostname, port) if a colon is present, or (input, nil) otherwise.
local function parse_host_port(host_port_str)
    local colon_pos = host_port_str:find(":", 1, true)
    if not colon_pos then
        return host_port_str, nil
    end
    return host_port_str:sub(1, colon_pos - 1),
           tonumber(host_port_str:sub(colon_pos + 1))
end

-- Gets or creates the DNS resolver.
-- Creates a new resolver for each request to avoid UDP socket request context
-- mismatch errors. The resolver contains UDP sockets that are bound to the
-- request context, so caching would cause "bad request" errors across requests.
-- Returns resolver on success, or (nil, error) on failure.
local function get_resolver(cache)
    local balancer_config, err = get_balancer_config(cache)
    if not balancer_config then return nil, err end

    local nameservers
    local bridge_resolver = ngx.var.bridge_resolver
    if bridge_resolver and bridge_resolver ~= "" then
        -- Separates hostname from port before DNS resolution. The bridgeResolver
        -- Helm value may include a port (e.g. "unbound.svc:53") which is not a
        -- valid DNS hostname. resty.dns.resolver accepts {ip, port} tables.
        local br_host, br_port = parse_host_port(bridge_resolver)
        local bridge_resolver_ip = resolve_bridge_dns(br_host, balancer_config, cache)
        if br_port then
            nameservers = {{bridge_resolver_ip, br_port}}
        else
            nameservers = {bridge_resolver_ip}
        end
    else
        -- Falls back to reading /etc/resolv.conf.
        local ns_err
        nameservers, ns_err = read_nameservers("/etc/resolv.conf", balancer_config.resolver.fallback)
        if ns_err then
            ngx.log(ngx.WARN, "⚠️ cannot read resolv.conf, using fallback: ", table.concat(balancer_config.resolver.fallback, ", "))
        end
    end

    -- The DNS resolver cannot be cached. It causes UDP sockets to be reused across different requests,
    -- leading to request context validation failures.
    local r, resolver_err = resolver:new{
        nameservers = nameservers,
        timeout = balancer_config.resolver.query.timeout_ms,
        retrans = balancer_config.resolver.query.retries,
        no_random = not balancer_config.resolver.query.random,
    }

    if not r then
        local detailed_err = "failed to create resolver (timeout_ms: " .. tostring(balancer_config.resolver.query.timeout_ms) ..
                ", retries: " .. tostring(balancer_config.resolver.query.retries) ..
                ", random: " .. tostring(balancer_config.resolver.query.random) ..
                "): " .. (resolver_err or "unknown error")
        return nil, detailed_err
    end

    return r
end

-- Parses upstream_target_proxy_pass to extract connection details.
-- Returns (hostname, port, is_tls) on success or (nil, nil, nil, error) on failure.
local function parse_upstream_target(upstream_target)
    -- Full URL with scheme (http://host or https://host:port).
    local scheme, hostname, port_str = upstream_target:match("^(%a+)://([^:/]+):?(%d*)$")

    if not hostname then
        return nil, nil, nil, "failed to parse upstream target (must be full URL with scheme): " .. upstream_target
    end

    -- Determines TLS and port from scheme.
    local is_tls = true
    local port = 443

    if scheme == "http" then
        is_tls = false
        port = 80
    end

    -- Overrides port if explicitly specified.
    if port_str and port_str ~= "" then
        local parsed_port = tonumber(port_str)
        if not parsed_port then
            return nil, nil, nil, "invalid port number in: " .. upstream_target
        end
        port = parsed_port
    end

    return hostname, port, is_tls
end

-- Resolves a hostname to A records via DNS.
-- Returns answer table on success, or nil on failure.
local function resolve_dns(hostname, cache)
    local r, err = get_resolver(cache)
    if not r then return nil, "cannot get resolver: " .. (err or "unknown error") end

    local ok, answers, query_err = pcall(function()
        return r:query(hostname, { qtype = r.TYPE_A })
    end)

    if not ok then
        return nil, "DNS query exception for hostname: " .. tostring(hostname) .. ", exception: " .. tostring(answers)
    end

    if not answers then
        return nil, "DNS lookup failed: " .. tostring(query_err) .. ", for: " .. hostname
    end

    if answers.errcode then
        return nil, "DNS lookup failed: " .. tostring(answers.errstr) .. ", for: " .. hostname
    end

    return answers
end

-- Resolves DNS for upstream keepalive connection pooling.
-- Returns {ip, port, hostname, is_tls} for the balancer phase, or nil on failure.
local function resolve_for_keepalive(upstream_target)
    if not upstream_target or upstream_target == "" then
        return nil
    end

    local cache = ngx.shared.upstreamBalancerCache
    local balancer_config, err = get_balancer_config(cache)
    if not balancer_config then
        ngx.log(ngx.ERR, "❌ cannot get balancer config: ", err)
        return nil
    end

    local hostname, port, is_tls, parse_err = parse_upstream_target(upstream_target)
    if not hostname then
        ngx.log(ngx.ERR, "❌ ", parse_err or "failed to parse upstream_target")
        return nil
    end

    local keys_records = "resolved_" .. hostname
    local records_string = cache:get(keys_records)

    if records_string == DNS_FAILURE_MARKER then
        ngx.log(ngx.ERR, "❌ cannot route request, no A records yet for failed DNS lookup: " .. hostname)
        return nil
    end

    if records_string == nil then
        local answers, dns_err = resolve_dns(hostname, cache)
        if not answers then
            ngx.log(ngx.ERR, "❌ ", dns_err or ("cannot route request, no A records: " .. hostname))
            -- Caches DNS query failures to prevent retry storms.
            cache:set(keys_records, DNS_FAILURE_MARKER, balancer_config.resolver.cache.failure_ttl_secs)
            return nil
        end

        local records = {}
        local len = 0

        for _, ans in ipairs(answers) do
            if ans.address then
                len = len + 1
                table.insert(records, ans.address)
            end
        end

        if len ~= 0 then
            records_string = table.concat(records, ",")
            cache:set(keys_records, records_string, balancer_config.resolver.cache.success_ttl_secs)
        else
            ngx.log(ngx.ERR, "❌ cannot route request, no A records: " .. hostname)
            cache:set(keys_records, DNS_FAILURE_MARKER, balancer_config.resolver.cache.failure_ttl_secs)
            return nil
        end
    end

    local records = {}

    for val in (records_string .. ","):gmatch("(.-),") do
        table.insert(records, val)
    end

    local idx = (dns_rr_rand % #records) + 1
    local ip = records[idx]
    -- NGINX workers is single-threaded, so no race conditions.
    dns_rr_rand = dns_rr_rand + 1

    return {
        ip = ip,
        port = port,
        hostname = hostname,
        is_tls = is_tls,
    }
end

-- Module exports.
local _M = {}

--- Resolves DNS for upstream keepalive connection pooling.
--- Parses the upstream target URL, performs DNS resolution with caching,
--- and selects an IP via round-robin from the resolved A records.
---
---@param upstream_target string The upstream target URL (e.g., "https://bucket.s3.amazonaws.com").
---@return table|nil peer Table with {ip, port, hostname, is_tls} or nil on failure.
---
--- Example:
---     local balancer = require "lua-balancer"
---     local peer = balancer.resolve_for_keepalive(ngx.var.upstream_target)
---     if peer then
---         ngx.ctx.upstream_peer = peer
---     else
---         ngx.log(ngx.ERR, "failed to resolve peer")
---         return ngx.exit(ngx.HTTP_BAD_GATEWAY)
---     end
_M.resolve_for_keepalive = resolve_for_keepalive
_M.parse_host_port = parse_host_port

return _M
