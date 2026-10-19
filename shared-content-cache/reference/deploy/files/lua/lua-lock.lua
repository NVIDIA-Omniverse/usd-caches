-- SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
-- SPDX-License-Identifier: Apache-2.0

--[[
This Lua module provides lock-based execution for reducing concurrent operations
on the same resource, similar to NGINX's proxy_cache_lock directive.

Features:
    - Lock acquisition with configurable timeout.
    - Metrics tracking for lock wait time and lock counts.
    - Graceful degradation when locks cannot be acquired.
    - Automatic lock cleanup on function completion.

Requires:
    - Shared dictionary `lock_by_request_uri_table` for lock storage.
    - lua-metrics module for observability.

Usage:
    local lock = require "lua-lock"

    local result = lock.with_lock(function()
        return expensive_operation()
    end, "my_resource_key", 5)
]]

local resty_lock = require "resty.lock"
local metrics = require "lua-metrics"

local _M = {}

--- Executes a function within a lock context to reduce concurrent executions.
--- The lock is best-effort; if acquisition fails, the function still executes
--- but logs a warning. This prevents blocking while reducing thundering herd.
---
---@param func function Function to execute within lock context.
---@param cache_key string Unique key for the lock (e.g., request URI, hostname).
---@param timeout? number Timeout in seconds for lock acquisition. Optional; defaults to 5.
---@return any|nil result Result from the function, or nil on error.
---@return any|nil error Error from the function if it failed.
---
--- Example:
---     local lock = require "lua-lock"
---     local result = lock.with_lock(function()
---         return make_expensive_call()
---     end, "dns_bucket.s3.amazonaws.com", 2)
function _M.with_lock(func, cache_key, timeout)
    timeout = timeout or 5

    -- Tries to create lock.
    local lock_obj, err = resty_lock:new("lock_by_request_uri_table", { timeout = timeout })

    local lock_acquired = false
    if lock_obj then
        local mutex_timer = metrics.timer(metrics.histogram.access_mutex_wait_time)
        local elapsed, lock_err = lock_obj:lock(cache_key)
        if elapsed ~= nil then
            lock_acquired = true
            metrics.gauge.access_mutex_locked:inc(1)
        else
            ngx.log(ngx.WARN, "⚠️ lock not acquired: ", lock_err)
        end

        mutex_timer({ ngx.var.host, lock_acquired })
    else
        ngx.log(ngx.WARN, "⚠️ failed to create lock: ", err)
    end

    -- Executes function regardless of lock status.
    local ok, result = pcall(func)

    -- Releases lock if it was acquired.
    if lock_acquired then
        local unlocked, unlock_err = lock_obj:unlock()
        if unlocked then
            metrics.gauge.access_mutex_locked:inc(-1)
        else
            -- Does not decrement the lock counter if the unlock fails.
            ngx.log(ngx.ERR, "❌ failed to unlock: ", unlock_err)
        end
    end

    if not ok then
        ngx.log(ngx.ERR, "❌ protected function failed: ", tostring(result))
        return nil, result
    end

    return result
end

return _M

