# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
"""Experimental path-only client. Ship with the native library from the same build."""

import asyncio
import ctypes as c
from dataclasses import dataclass
from enum import IntEnum


class Outcome(IntEnum):
    READY = 0
    NOT_LEASABLE = 1
    UNSUPPORTED_LAYOUT = 2
    REJECTED = 3
    EXPIRED = 4


@dataclass(frozen=True, repr=False)
class Request:
    authority: str
    path: str
    query: str = ""
    headers: tuple[tuple[str, str], ...] = ()
    nucleus: bool = False
    lease_duration_millis: int = 30_000


@dataclass(frozen=True)
class Response:
    outcome: Outcome
    status: int
    generation: int
    offset: int
    length: int
    deadline_unix_millis: int
    path: str | None
    cache_status: str | None
    headers: tuple[tuple[str, str], ...]


class Error(OSError):
    def __init__(self, code, message):
        super().__init__(message)
        self.code = code


class _Header(c.Structure):
    _fields_ = [("name", c.c_char_p), ("value", c.c_char_p)]


class _Request(c.Structure):
    _fields_ = [
        ("authority", c.c_char_p), ("path", c.c_char_p), ("query", c.c_char_p),
        ("headers", c.POINTER(_Header)), ("header_count", c.c_size_t),
        ("nucleus", c.c_uint8), ("lease_duration_millis", c.c_uint64),
    ]


class _Error(c.Structure):
    _fields_ = [("code", c.c_int32), ("message", c.c_char * 256)]


class _View(c.Structure):
    _fields_ = [
        ("outcome", c.c_uint32), ("status", c.c_uint16), ("generation", c.c_uint64),
        ("offset", c.c_uint64), ("length", c.c_uint64),
        ("deadline_unix_millis", c.c_uint64), ("path", c.c_char_p),
        ("cache_status", c.c_char_p), ("headers", c.POINTER(_Header)),
        ("header_count", c.c_size_t),
    ]


def _text(value):
    encoded = value.encode("utf-8")
    if b"\0" in encoded:
        raise ValueError("request strings cannot contain NUL")
    return encoded


def _decode(value):
    return None if value is None else value.decode("utf-8")


class _Native:
    def __init__(self, library, timeout_ms, max_connections):
        self.lib = c.CDLL(str(library))
        self.pointer = c.c_void_p()
        signatures = {
            "ovcc_local_abi_version": ([], c.c_uint32),
            "ovcc_local_client_new_v1": ([c.c_uint64, c.POINTER(c.c_void_p), c.POINTER(_Error)], c.c_int32),
            "ovcc_local_client_new_with_pool_v1": ([c.c_uint64, c.c_size_t, c.POINTER(c.c_void_p), c.POINTER(_Error)], c.c_int32),
            "ovcc_local_get_v1": ([c.c_void_p, c.POINTER(_Request), c.POINTER(c.c_void_p), c.POINTER(_Error)], c.c_int32),
            "ovcc_local_client_free_v1": ([c.c_void_p], None),
            "ovcc_local_response_view_v1": ([c.c_void_p, c.POINTER(_View)], None),
            "ovcc_local_response_free_v1": ([c.c_void_p], None),
        }
        for name, (args, result) in signatures.items():
            function = getattr(self.lib, name)
            function.argtypes, function.restype = args, result
        if self.lib.ovcc_local_abi_version() != 1:
            raise RuntimeError("incompatible OVCC native bridge")
        failure = _Error()
        if self.lib.ovcc_local_client_new_with_pool_v1(timeout_ms, max_connections, c.byref(self.pointer), c.byref(failure)):
            raise Error(failure.code, _decode(failure.message))

    def __del__(self):
        pointer = getattr(self, "pointer", None)
        if pointer:
            self.lib.ovcc_local_client_free_v1(pointer)


class Client:
    """Owns a shared bounded pool; never opens, maps or retries a file."""

    def __init__(self, library, timeout_ms=30_000, max_connections=4):
        if not isinstance(timeout_ms, int) or not 1 <= timeout_ms <= 86_400_000:
            raise ValueError("timeout_ms must be between 1 and 86400000")
        if not isinstance(max_connections, int) or not 1 <= max_connections < 1 << (8 * c.sizeof(c.c_size_t)):
            raise ValueError("max_connections must be a positive native-size integer")
        self._native = _Native(library, timeout_ms, max_connections)

    def get(self, request: Request) -> Response:
        if not isinstance(request.nucleus, bool):
            raise ValueError("nucleus must be a boolean")
        duration = request.lease_duration_millis
        if not isinstance(duration, int) or not 1 <= duration <= 300_000:
            raise ValueError("lease_duration_millis must be between 1 and 300000")
        if len(request.headers) > 1024:
            raise ValueError("too many headers")
        fields = (_Header * len(request.headers))(
            *(_Header(_text(name), _text(value)) for name, value in request.headers))
        wire = _Request(_text(request.authority), _text(request.path), _text(request.query),
                        fields, len(fields), request.nucleus, duration)
        owner = self._native
        raw, failure = c.c_void_p(), _Error()
        if owner.lib.ovcc_local_get_v1(owner.pointer, c.byref(wire), c.byref(raw), c.byref(failure)):
            raise Error(failure.code, _decode(failure.message))
        try:
            view = _View()
            owner.lib.ovcc_local_response_view_v1(raw, c.byref(view))
            return Response(Outcome(view.outcome), view.status, view.generation,
                            view.offset, view.length, view.deadline_unix_millis,
                            _decode(view.path), _decode(view.cache_status),
                            tuple((_decode(view.headers[i].name), _decode(view.headers[i].value))
                                  for i in range(view.header_count)))
        finally:
            owner.lib.ovcc_local_response_free_v1(raw)

    async def get_async(self, request: Request) -> Response:
        # Cancellation stops waiting; the timeout-bounded worker retains its client.
        return await asyncio.to_thread(self.get, request)
