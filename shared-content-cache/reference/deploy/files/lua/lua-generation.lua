-- SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
-- SPDX-License-Identifier: Apache-2.0

--[[
Implements a generation reset endpoint using compare-and-set semantics.

This endpoint provides a monotonically increasing generation number for clients
to coordinate cache invalidation across a distributed, multi-replica service.
It avoids the need for an external coordination service.

Two-phase update protocol:
    1) Request without `gen`:
    Returns the current generation number for the client to use as a baseline.

    2) Request with `gen=<new_number>`:
    Updates the generation only if `<new_number>` is strictly greater than the
    current value. Ensures monotonicity. On success, returns the new value and
    `old_generation`. If rejected, returns the current value with an error.

Concurrency:
    A local mutex serializes access to the shared generation value, preventing
    races between NGINX worker processes on the same replica.

API Contract:
    - Always returns a JSON response.
    - Always includes the `{{ include "func.instanceHeader" . }}` HTTP header.
    - Rejects non-monotonic updates with an explanatory error.
]]

local metrics = require "lua-metrics"
local lock_module = require "lua-lock"

local function get_instance()
    local h = ngx.var.hostname
    assert(h ~= nil and h ~= "", "hostname not set")
    return h
end

local function json_respond(instance, generation, old_generation, error_str)
    -- Header names are templated by Helm. Long-bracket strings keep this file
    -- valid Lua even when unrendered (so busted can load it); the legacy header
    -- is gated on a runtime non-empty check instead of a Helm if/end block, so
    -- there are no control-flow templates that would break raw loading.
    local instance_header = [[{{ include "func.instanceHeader" . }}]]
    ngx.header[instance_header] = instance
    local legacy_instance_header = [[{{ include "func.legacyInstanceHeader" . }}]]
    if legacy_instance_header ~= "" then
        ngx.header[legacy_instance_header] = instance
    end
    ngx.header["Content-Type"] = "application/json"
    local response = {
        instance = instance,
        generation = generation
    }

    if old_generation then
        response.old_generation = old_generation
    end

    if error_str then
        response.error = error_str
    end

    local cjson = require "cjson"
    ngx.say(cjson.encode(response))
end

-- Returns current generation string; defaults to "0" if unset or invalid.
local function get_current_generation(dict)
    local current = dict:get("generation")
    if not current or current == "" then
        return "0"
    end
    return tostring(current)
end

-- Executes a function within a lock context for cache generation operations.
-- Uses the lua-lock module with stricter error handling for generation updates.
local function with_lock(func, key, timeout)
    timeout = timeout or 3

    local result, err = lock_module.with_lock(func, key, timeout)

    if not result and err then
        return false, { error = "execution_failed" }
    end

    return true, result
end

-- Handles the request.
local function handle()
    local timer = metrics.timer(metrics.histogram.access_time)
    local args = ngx.req.get_uri_args()
    local dict = ngx.shared.cc_generation
    local instance = get_instance()

    local gen = args["gen"]
    if not gen or gen == "" then
        metrics.counter.access_request:inc(1, { ngx.var.host or "", "GEN_READ" })
        local current_gen = get_current_generation(dict)
        timer({ ngx.var.host or "" })
        return json_respond(instance, current_gen)
    end

    -- Parses requested generation.
    local requested_num = tonumber(gen)
    if not requested_num then
        local current_gen = get_current_generation(dict)
        timer({ ngx.var.host or "" })
        return json_respond(instance, current_gen, nil, "invalid_generation")
    end

    metrics.counter.access_request:inc(1, { ngx.var.host or "", "GEN_WRITE" })

    local ok, result = with_lock(function()
        local current_gen = get_current_generation(dict)
        local current_num = tonumber(current_gen) or 0
        if current_num < requested_num then
            dict:set("generation", tostring(requested_num))
            return { status = "updated", new_gen = tostring(requested_num), old_gen = current_gen }
        else
            return { status = "no_update", current_gen = current_gen }
        end
    end, "cc_generation_reset", 3)

    if not ok then
        local current_gen = get_current_generation(dict)
        local err = (result and result.error) or "lock_error"
        metrics.counter.access_response:inc(1, { ngx.var.host or "", "GEN_WRITE", 200 })
        timer({ ngx.var.host or "" })
        return json_respond(instance, current_gen, nil, err)
    end

    if result and result.status == "updated" then
        metrics.counter.access_response:inc(1, { ngx.var.host or "", "GEN_WRITE", 200 })
        timer({ ngx.var.host or "" })
        return json_respond(instance, result.new_gen, result.old_gen, nil)
    else
        local current = (result and (result.current_gen or result.old_gen)) or get_current_generation(dict)
        metrics.counter.access_response:inc(1, { ngx.var.host or "", "GEN_WRITE", 200 })
        timer({ ngx.var.host or "" })
        return json_respond(instance, current, nil, "generation_not_updated")
    end
end

if _G._UNIT_TEST == true then
    return {
        handle = handle,
        with_lock = with_lock,
        get_current_generation = get_current_generation,
    }
else
    return handle()
end
