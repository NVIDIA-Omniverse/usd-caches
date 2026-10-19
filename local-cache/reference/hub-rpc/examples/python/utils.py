# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

import random
import string
import uuid

from local_cache_pb2 import Request


def next_request_id() -> int:
    next_request_id.val += 1
    return next_request_id.val
next_request_id.val = 0


def get_client_uuid() -> str:
    return get_client_uuid.val
get_client_uuid.val = str(uuid.uuid4())


def generate_str(len) -> str:
    return ''.join(random.choices(string.ascii_letters + string.digits, k=len))


def generate_bytes(len) -> bytes:
    return generate_str(len).encode()


def generate_key(len=10) -> str:
    return generate_str(len)


def create_request() -> Request:
    request = Request()
    request.request_id = next_request_id()
    return request
