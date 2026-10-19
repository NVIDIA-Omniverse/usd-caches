-- SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
-- SPDX-License-Identifier: Apache-2.0

--[[
Prometheus metric constructors for OVCC request, cache, and Lua runtime stats.

The prefix is templated by Helm (func.metricPrefix). A Lua long-bracket string
keeps this file valid Lua even when unrendered, so busted unit tests can load
the module directly; Helm still substitutes the template on render.
]]

local prometheus = require("prometheus").init("cache_metrics", {prefix = [[{{ include "func.metricPrefix" . }}]]})

local histogram_time_buckets = {0.005, 0.01, 0.02, 0.03, 0.05, 0.075, 0.1, 0.2, 0.3, 0.4, 0.5, 0.75, 1, 1.5, 2, 3, 4, 5, 10}
local histogram_bytes_buckets = {1000,2000,4000,8000,16000,32000,64000,128000,256000,1048576, 10485760, 104857600, 1000000000, 10000000000, 100000000000}

local lua_exception = prometheus:counter("lua_exception_total", "description", {"name"})
local lua_ngx_exit = prometheus:counter("lua_ngx_exit_total", "description", {"phase", "http_status"})

local metric_nginx_shared_dict_info = prometheus:gauge(
    "nginx_shared_dict_info",
    "Provides current information on lua dictionaries.",
    {"name", "stat"})

function ctor()
    return {
        counter = {
            http_requests = prometheus:counter(
                "http_requests_total",
                "Count of requests with additional labels",
                {"http_host", "request_method", "cache_status", "http_status"}),
            http_upstream_requests = prometheus:counter(
                "http_upstream_requests_total",
                "Count of upstream requests with additional labels",
                {"http_host", "request_method", "cache_status", "http_status"}),

            access_request = prometheus:counter(
                "access_requests_total",
                "Total number of requests made by the access lua script",
                {"http_host", "http_method"}),
            access_response = prometheus:counter(
                "access_responses_total",
                "Total number of responses from requests made by access lua script.",
                {"http_host", "http_method", "http_status"}),

            mutation_detection_purges = prometheus:counter(
                "mutation_detection_purges_total",
                "Cache entries purged due to a stale upstream validator",
                {"http_host"}),

            bytes_tx = prometheus:counter(
                "tx_bytes_total",
                "Total number of bytes sent.",
                {  }),
            bytes_rx = prometheus:counter(
                "rx_bytes_total",
                "Total number of bytes received.",
                {  }),

            poll_total = prometheus:counter(
                "metric_poll_total",
                "Total number of requests to the http metrics endpoint.",
                {  }),

            lua_exception = lua_exception,
            lua_ngx_exit = lua_ngx_exit,
        },
        gauge = {
            connections = prometheus:gauge(
                "nginx_http_connections",
                "Number of HTTP connections",
                {"state"}),
            access_mutex_locked = prometheus:gauge(
                "access_mutex_locks",
                "The number of active mutex locks.",
                {}),

            workers = prometheus:counter(
                "nginx_workers", "", nil),
        },
        histogram = {
            request_duration_seconds = prometheus:histogram(
                "request_duration_seconds",
                "HTTP request latency",
                {"http_host"}, histogram_time_buckets),

            upstream_connect_time = prometheus:histogram(
                "upstream_connect_time_seconds",
                "Time spent connecting to upstream services",
                {"http_host"}, histogram_time_buckets),
            upstream_header_time = prometheus:histogram(
                "upstream_header_time_seconds",
                "Time to read headers from upstream",
                {"http_host"}, histogram_time_buckets),
            upstream_response_time = prometheus:histogram(
                "upstream_response_time_seconds",
                "Time to receive all bytes from upstream",
                {"http_host"}, histogram_time_buckets),
            upstream_queue_time = prometheus:histogram(
                "upstream_queue_time_seconds",
                "Amount of time time spent in connection queue",
                {"http_host"}, histogram_time_buckets),

            upstream_bytes_sent = prometheus:histogram(
                "upstream_tx_bytes",
                "Number of bytes sent",
                {"http_host"}, histogram_bytes_buckets),
            upstream_bytes_received = prometheus:histogram(
                "upstream_rx_bytes",
                "Number of bytes recieved",
                {"http_host"}, histogram_bytes_buckets),

            tcpinfo_rtt = prometheus:histogram(
                "tcpinfo_rtt_seconds",
                "The TCP socket RTT time.",
                {"http_host"}, histogram_time_buckets),
            tcpinfo_rttvar = prometheus:histogram(
                "tcpinfo_rttvar_seconds",
                "The TCP socker RTT variance",
                {"http_host"}, histogram_time_buckets),

            access_time = prometheus:histogram(
                "access_handler_seconds",
                "Time spent by access lua script.",
                {"http_host"}, histogram_time_buckets),

            access_mutex_wait_time = prometheus:histogram(
                "access_mutex_acquire_seconds",
                "Time spent by access lua script to obtain a mutex lock.",
                {"http_host", "lock_acquired"}, histogram_time_buckets),
            access_mutex_locked_time = prometheus:histogram(
                "access_mutex_locked_seconds",
                "Time spent by access lua script during a protected region.",
                {"http_host"}, histogram_time_buckets),
            access_upstream_request_time = prometheus:histogram(
                "access_upstream_request_seconds",
                "Time spent by access lua scripts performing requests.",
                {"http_host"}, histogram_time_buckets),

            tcpinfo_snd_cwnd = prometheus:histogram(
                "tcpinfo_snd_cwnd_bytes",
                "Tcp info: size send window",
                {"http_host"}, histogram_bytes_buckets),

            response_size_bytes = prometheus:histogram(
                "response_size_bytes",
                "Size of HTTP responses",
                {"http_host"}, histogram_bytes_buckets),
        },
    }
end

local metrics = {}

metrics.pcall = function (func)
    local value, err = pcall(func)
    if err ~= nil then
        local err_code = -1
        local err_msg = "nil"

        if err.message ~= nil then
            err_msg = err.message
        end
        if err.code ~= nil then
            err_code = err.code
        end

        lua_exception:incr(1, { err_code, err_msg })
    end
    return value, err
end

metrics.timer = function (metric)
    ngx.update_time()
    local time = ngx.now()
    return function(label_values)
        ngx.update_time()
        local diff = ngx.now() - time
        metric:observe(diff, label_values)
    end
end

metrics.report_on_dict = function (name, dict)
    metric_nginx_shared_dict_info:set(dict:free_space(), { name, "free_space" })
    metric_nginx_shared_dict_info:set(dict:capacity(), { name, "capacity" })
end

local ngx_exit = ngx.exit
ngx.exit = function (status)
    local phase = ngx.get_phase()
    lua_ngx_exit:inc(1, { phase, status })
    ngx_exit(status)
end

metrics.prometheus = prometheus

metrics.init = function(dict)
    if dict['time_bucket'] ~= nil then
        histogram_time_buckets = dict.time_bucket
    end
    if dict['byte_bucket'] ~= nil then
        histogram_bytes_buckets = dict.byte_bucket
    end
    for k, v in pairs(ctor()) do
        metrics[k] = v
    end
end

return metrics
