# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

import asyncio
from asyncio import create_task
from asyncio import Future, Queue, StreamReader, StreamWriter
import mmap
import struct
import utils

import local_cache_pb2
from local_cache_pb2 import Request, Response


def send_msg(writer: StreamWriter, data: bytes) -> None:
    """Send a framed message."""
    header = struct.pack(">I", len(data))
    writer.write(header + data)


async def recv_msg(reader: StreamReader) -> bytes:
    """Receive a framed message."""
    raw_header = await reader.readexactly(4)
    msg_len = struct.unpack(">I", raw_header)[0]
    return await reader.readexactly(msg_len)


class Client:
    """
    This class uses an asyncio.Queue[Request, Future] as a write pipeline.
    New requests are submitted to the queue and written by a dedicated
    writer_task(). Responses are read by a dedicated reader_task() and passed
    to the associated Future.
    """

    def __init__(self, reader: StreamReader, writer: StreamWriter):
        self.reader = reader
        self.writer = writer
        self.pipeline = Queue()
        self.pending = {}
        self.reader_task = create_task(reader_task(self.pending, self.reader))
        self.writer_task = create_task(writer_task(self.pipeline, self.writer))

    @staticmethod
    async def connect(host: str, port: int) -> 'Client':
        """Create a Client instance for host:port"""
        reader, writer = await asyncio.open_connection(host, port)
        return Client(reader, writer)

    async def close(self):
        """Cleanly shutdown this client instance."""

        # Signal the writer to terminate. Note that we could use shutdown() in
        # Python 3.13+ instead, which would also block new requests
        await self.pipeline.put((None, None))
        # Wait for the pipeline to drain
        await self.pipeline.join()
        # By now, the writer should be done
        await self.writer_task
        # Wait for the list of pending tasks to drain
        while len(self.pending) > 0:
            asyncio.sleep(1)
        # Close the stream to signal the reader to terminate
        self.writer.close()
        await self.writer.wait_closed()
        # Wait for the reader to terminate
        await self.reader_task

    async def submit(self, request: Request) -> Future[Response]:
        """
        Submit a new request to the connected Local Cache server by enqueuing
        it in the write pipeline. The returned future will signal when the
        response has been received.

        It is important to note that the Local Cache server may process
        requests out of order, and so we match responses to requests here
        using the request_id field.
        """
        future = asyncio.get_running_loop().create_future()
        self.pending[request.request_id] = future
        await self.pipeline.put((request, future))
        return future


async def reader_task(pending: dict, reader: StreamReader):
    """
    Loop on reading Response messages from the reader stream, and signaling
    the associated future. Interpret a close event as a signal to terminate.
    """
    while True:
        try:
            response_data = await recv_msg(reader)
            response = Response()
            response.ParseFromString(response_data)
            future = pending.pop(response.request_id)
            future.set_result(response)
        except asyncio.IncompleteReadError:
            for key, val in pending:
                val.set_exception(asyncio.IncompleteReadError)
            return


async def writer_task(requests: Queue[(Request, Future)], writer: StreamWriter):
    """
    Loop on getting Requests from the queue and writing them to the stream.
    Interpret request=None as a signal to terminte. Note that in Python 3.13+
    we could call shutdown() on the queue instead and trap QueueShutDown.
    """
    while True:
        request, future = await requests.get()
        if request is None:
            requests.task_done()
            return
        request_data = request.SerializeToString()
        send_msg(writer, request_data)
        requests.task_done()


async def get_service_version_info(client: Client) -> str:
    request = utils.create_request()
    request.params.service_version_info.SetInParent()
    future = await client.submit(request)
    await future
    response = future.result()
    return str(response.result)


async def client_connect(client: Client) -> str:
    request = utils.create_request()
    request.params.client_connect.client_name = "async_client"
    request.params.client_connect.client_uuid = utils.get_client_uuid()
    future = await client.submit(request)
    await future
    return str(future.result())


async def cache_aside_open_for_write(client: Client, key: str, size: int) -> local_cache_pb2.CacheAsideOpenForWriteResult:
    request = utils.create_request()
    request.params.cache_aside_open_for_write.key = key
    request.params.cache_aside_open_for_write.size = size
    future = await client.submit(request)
    await future
    return future.result().result.cache_aside_open_for_write


async def cache_aside_close(client: Client, handle: str) -> str:
    request = utils.create_request()
    request.params.cache_aside_close.handle = handle
    future = await client.submit(request)
    await future
    return future.result().result


async def cache_aside_write(client: Client, key: str, data: bytes) -> str:
    open_result = await cache_aside_open_for_write(client, key, len(data))
    with open(open_result.local_file, "wb") as file:
        file.write(data)
    await cache_aside_close(client, open_result.handle)
    return open_result.local_file


async def cache_aside_open_for_read(client: Client, key: str) -> local_cache_pb2.CacheAsideOpenForReadResult:
    request = utils.create_request()
    request.params.cache_aside_open_for_read.key = key
    future = await client.submit(request)
    await future
    return future.result().result.cache_aside_open_for_read


async def cache_aside_read(client: Client, key: str) -> bytes:
    open_result = await cache_aside_open_for_read(client, key)
    with open(open_result.local_file, "rb") as file:
        # NOTE: The cache protocol passes file paths rather than content so
        #       that files can be memory-mapped. In a real application, a file
        #       would be held open for as long as its content was mapped, but
        #       for the sake of this example, we're just returning a copy of
        #       the data.
        mm = mmap.mmap(file.fileno(), 0, prot=mmap.PROT_READ)
        file_data = mm.read()
        mm.close()
    await cache_aside_close(client, open_result.handle)
    return file_data


async def run():
    client = await Client.connect('127.0.0.1', 14093)

    print(await get_service_version_info(client))

    await client_connect(client)

    key = utils.generate_key()
    data_out = utils.generate_bytes(100)
    await cache_aside_write(client, key, data_out)
    results = await asyncio.gather(
        cache_aside_read(client, key),
        cache_aside_read(client, key),
        cache_aside_read(client, key),
        cache_aside_read(client, key),
        cache_aside_read(client, key))
    for data_in in results:
        assert(data_in == data_out)

    await client.close()


if __name__ == "__main__":
    asyncio.run(run())
