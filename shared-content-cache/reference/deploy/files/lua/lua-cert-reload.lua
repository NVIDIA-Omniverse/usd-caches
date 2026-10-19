-- SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
-- SPDX-License-Identifier: Apache-2.0

--[[
TLS certificate hot-reload helpers for OVCC.

Watches the mounted certificate and key paths, reloads them into the worker
when their identity changes, and fails closed on unrecoverable read errors.
]]

local M = {}

local CERT_PATH = "/etc/nginx/ssl/tls.crt"
local KEY_PATH = "/etc/nginx/ssl/tls.key"

local worker_cache = {
    identity = nil,
    cert = nil,
    priv_key = nil,
}

local ffi_module = nil
local STAT_CDEF_LOADED_KEY = "__ovcc_tls_cert_reload_stat_cdef_loaded"
local stat_identity_override = nil
local last_passthrough_error = nil

local function log(level, ...)
    if ngx and ngx.log then
        ngx.log(level, ...)
    end
end

local function exit_error()
    if ngx and ngx.exit and ngx.ERROR then
        return ngx.exit(ngx.ERROR)
    end
    return nil
end

local function passthrough_on_error(err)
    local message = tostring(err)
    if message ~= last_passthrough_error then
        log(ngx.WARN, "tls cert reload passthrough: ", message)
        last_passthrough_error = message
    end
    return true
end

local function get_ssl()
    local ok, ssl = pcall(require, "ngx.ssl")
    if not ok then
        return nil, ssl
    end
    return ssl, nil
end

local function get_ffi()
    if ffi_module then
        return ffi_module, nil
    end

    local ok, ffi = pcall(require, "ffi")
    if not ok then
        return nil, ffi
    end

    if not _G[STAT_CDEF_LOADED_KEY] then
        -- stat(2): https://man7.org/linux/man-pages/man2/stat.2.html
        local ok_cdef, cdef_err = pcall(ffi.cdef, [[
            typedef long ovcc_tls_time_t;

            typedef struct {
                ovcc_tls_time_t tv_sec;
                long tv_nsec;
            } ovcc_tls_timespec_t;

            typedef struct {
                unsigned long st_dev;
                unsigned long st_ino;
                unsigned long st_nlink;
                unsigned int st_mode;
                unsigned int st_uid;
                unsigned int st_gid;
                int __pad0;
                unsigned long st_rdev;
                long st_size;
                long st_blksize;
                long st_blocks;
                ovcc_tls_timespec_t st_atim;
                ovcc_tls_timespec_t st_mtim;
                ovcc_tls_timespec_t st_ctim;
                long __glibc_reserved[3];
            } ovcc_tls_stat_t;

            int stat(const char *pathname, ovcc_tls_stat_t *statbuf);
        ]])
        if not ok_cdef then
            return nil, "failed to load stat ffi declarations: " .. tostring(cdef_err)
        end
        _G[STAT_CDEF_LOADED_KEY] = true
    end

    ffi_module = ffi
    return ffi_module, nil
end

local function stat_identity(path)
    if stat_identity_override then
        local ok, identity, err = pcall(stat_identity_override, path)
        if not ok then
            return nil, "stat identity override failed: " .. tostring(identity)
        end
        return identity, err
    end

    local ffi, err = get_ffi()
    if not ffi then
        return nil, "failed to load ffi: " .. tostring(err)
    end

    local ok_new, buf = pcall(ffi.new, "ovcc_tls_stat_t[1]")
    if not ok_new then
        return nil, "failed to allocate stat buffer: " .. tostring(buf)
    end

    local ok_stat, stat_rc = pcall(ffi.C.stat, path, buf)
    if not ok_stat then
        return nil, "stat(" .. path .. ") failed: " .. tostring(stat_rc)
    end
    if stat_rc ~= 0 then
        local errno = ffi.errno and ffi.errno() or "unknown"
        return nil, "stat(" .. path .. ") failed: errno=" .. tostring(errno)
    end

    local st = buf[0]
    return table.concat({
        tostring(tonumber(st.st_ino)),
        tostring(tonumber(st.st_size)),
        tostring(tonumber(st.st_mtim.tv_sec)),
        tostring(tonumber(st.st_mtim.tv_nsec)),
        tostring(tonumber(st.st_mode)),
    }, ":")
end

local function current_identity()
    local cert_id, cert_err = stat_identity(CERT_PATH)
    if not cert_id then
        return nil, cert_err
    end

    local key_id, key_err = stat_identity(KEY_PATH)
    if not key_id then
        return nil, key_err
    end

    return cert_id .. "|" .. key_id
end

local function read_file(path)
    local file, err = io.open(path, "rb")
    if not file then
        return nil, "failed to open " .. path .. ": " .. tostring(err)
    end

    local data, read_err = file:read("*a")
    file:close()

    if data == nil then
        return nil, "failed to read " .. path .. ": " .. tostring(read_err)
    end

    return data, nil
end

local function parse_cert_pair(ssl, cert_pem, key_pem)
    local cert, cert_err = ssl.parse_pem_cert(cert_pem)
    if not cert then
        return nil, nil, "failed to parse TLS cert: " .. tostring(cert_err)
    end

    local priv_key, key_err = ssl.parse_pem_priv_key(key_pem)
    if not priv_key then
        return nil, nil, "failed to parse TLS private key: " .. tostring(key_err)
    end

    return cert, priv_key, nil
end

local function load_current_pair(ssl)
    local last_err = nil

    for _ = 1, 2 do
        local before, before_err = current_identity()
        if not before then
            return nil, nil, nil, before_err
        end

        local cert_pem, cert_read_err = read_file(CERT_PATH)
        if not cert_pem then
            return nil, nil, nil, cert_read_err
        end

        local key_pem, key_read_err = read_file(KEY_PATH)
        if not key_pem then
            return nil, nil, nil, key_read_err
        end

        local after, after_err = current_identity()
        if not after then
            return nil, nil, nil, after_err
        end

        if before == after then
            local cert, priv_key, parse_err = parse_cert_pair(ssl, cert_pem, key_pem)
            if not cert then
                return nil, nil, nil, parse_err
            end
            return after, cert, priv_key, nil
        end

        last_err = "TLS secret changed while loading cert pair; retrying"
    end

    return nil, nil, nil, last_err
end

function M.apply()
    local ssl, ssl_err = get_ssl()
    if not ssl then
        return passthrough_on_error(ssl_err)
    end

    local identity, identity_err = current_identity()
    if not identity then
        return passthrough_on_error(identity_err)
    end

    if worker_cache.identity ~= identity then
        local loaded_identity, cert, priv_key, load_err = load_current_pair(ssl)
        if not loaded_identity then
            return passthrough_on_error(load_err)
        end

        worker_cache.identity = loaded_identity
        worker_cache.cert = cert
        worker_cache.priv_key = priv_key
        last_passthrough_error = nil
    end

    local ok, err = ssl.clear_certs()
    if not ok then
        log(ngx.ERR, "tls cert reload failed to clear fallback certs: ", err)
        return exit_error()
    end

    ok, err = ssl.set_cert(worker_cache.cert)
    if not ok then
        log(ngx.ERR, "tls cert reload failed to set cert: ", err)
        return exit_error()
    end

    ok, err = ssl.set_priv_key(worker_cache.priv_key)
    if not ok then
        log(ngx.ERR, "tls cert reload failed to set private key: ", err)
        return exit_error()
    end

    return true
end

if _G._UNIT_TEST then
    function M._reset()
        worker_cache.identity = nil
        worker_cache.cert = nil
        worker_cache.priv_key = nil
        ffi_module = nil
        _G[STAT_CDEF_LOADED_KEY] = nil
        stat_identity_override = nil
        last_passthrough_error = nil
    end

    function M._set_paths(cert_path, key_path)
        CERT_PATH = cert_path
        KEY_PATH = key_path
    end

    function M._set_stat_identity(fn)
        stat_identity_override = fn
    end

    function M._current_identity()
        return current_identity()
    end
end

return M
