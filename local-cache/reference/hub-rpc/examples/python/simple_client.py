# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

import mmap
import socket
import struct
import utils

import local_cache_pb2
from local_cache_pb2 import Request, Response


def send_msg(sock: socket.socket, data: bytes) -> None:
    """Send a framed message."""
    header = struct.pack(">I", len(data))
    sock.sendall(header + data)


def recv_msg(sock: socket.socket) -> bytes:
    """Receive a framed message."""
    raw_header = _recvn(sock, 4)
    if len(raw_header) < 4:
        raise ConnectionError("connection closed")
    msg_len = struct.unpack(">I", raw_header)[0]
    return _recvn(sock, msg_len)


def _recvn(sock: socket.socket, n: int) -> bytes:
    """Read exactly n bytes."""
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            break
        buf.extend(chunk)
    return bytes(buf)


def rpc(sock: socket.socket, request: Request) -> Response:
    """Perform a blocking RPC operation."""
    request_data = request.SerializeToString()
    send_msg(sock, request_data)
    response_data = recv_msg(sock)
    response = Response()
    response.ParseFromString(response_data)
    assert(response.request_id == request.request_id)
    return response


def get_service_version_info(sock: socket.socket) -> str:
    request = utils.create_request()
    request.params.service_version_info.SetInParent()
    response = rpc(sock, request)
    return str(response.result)


def client_connect(sock: socket.socket) -> str:
    request = utils.create_request()
    request.params.client_connect.client_name = "simple_client"
    request.params.client_connect.client_uuid = utils.get_client_uuid()
    response = rpc(sock, request)
    return str(response.result)


def cache_aside_open_for_write(sock: socket.socket, key: str, size: int) -> local_cache_pb2.CacheAsideOpenForWriteResult:
    request = utils.create_request()
    request.params.cache_aside_open_for_write.key = key
    request.params.cache_aside_open_for_write.size = size
    response = rpc(sock, request)
    return response.result.cache_aside_open_for_write


def cache_aside_close(sock: socket.socket, handle: str) -> str:
    request = utils.create_request()
    request.params.cache_aside_close.handle = handle
    response = rpc(sock, request)
    return response.result


def cache_aside_write(sock: socket.socket, key: str, data: bytes) -> str:
    open_result = cache_aside_open_for_write(sock, key, len(data))
    with open(open_result.local_file, "wb") as file:
        file.write(data)
    cache_aside_close(sock, open_result.handle)
    return open_result.local_file


def cache_aside_open_for_read(sock: socket.socket, key: str) -> local_cache_pb2.CacheAsideOpenForReadResult:
    request = utils.create_request()
    request.params.cache_aside_open_for_read.key = key
    response = rpc(sock, request)
    return response.result.cache_aside_open_for_read


def cache_aside_read(sock: socket.socket, key: str) -> bytes:
    open_result = cache_aside_open_for_read(sock, key)
    with open(open_result.local_file, "rb") as file:
        # NOTE: The cache protocol passes file paths rather than content so
        #       that files can be memory-mapped. In a real application, a file
        #       would be held open for as long as its content was mapped, but
        #       for the sake of this example, we're just returning a copy of
        #       the data.
        mm = mmap.mmap(file.fileno(), 0, prot=mmap.PROT_READ)
        file_data = mm.read()
        mm.close()
    cache_aside_close(sock, open_result.handle)
    return file_data


if __name__ == "__main__":
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.connect(("127.0.0.1", 14093))

        print(get_service_version_info(sock))

        client_connect(sock)

        key = utils.generate_key()
        data_out = utils.generate_bytes(100)
        cache_aside_write(sock, key, data_out)
        data_in = cache_aside_read(sock, key)
        assert(data_in == data_out)

