"""Python receiver for the CrabGrab Windows -> WSL frame bridge (protocol v1).

Yields frames as NumPy arrays so they can go straight into OpenCV / PyTorch / an inference client.

    from crabgrab_bridge import FrameServer

    with FrameServer(port=7878) as server:       # listen inside WSL
        for frame in server.frames():            # blocks until the Windows sender connects
            bgra = frame.bgra                    # (H, W, 4) uint8 view, no copy
            rgb = frame.rgb()                    # (H, W, 3) uint8 copy
            print(frame.frame_id, frame.timestamp_s, bgra.shape)

Command line:  python3 crabgrab_bridge.py [--port 7878] [--max-frames N]

The wire format is defined in bridge/receiver/src/protocol.rs; this file mirrors it.
"""

from __future__ import annotations

import argparse
import socket
import struct
import sys
import time
from dataclasses import dataclass
from typing import Iterator, Optional

import numpy as np

MAGIC = b"CGRB"
VERSION = 1
DEFAULT_PORT = 7878
HELLO = struct.Struct("<4sHH")  # magic, version, flags
HEADER = struct.Struct("<IIIIQQQ")  # kind, width, height, stride, frame_id, timestamp_ns, payload_len
KIND_VIDEO_BGRA8 = 1
KIND_END = 2
MAX_DIMENSION = 16_384


class ProtocolError(Exception):
    pass


@dataclass
class Frame:
    frame_id: int
    timestamp_ns: int
    width: int
    height: int
    stride: int
    bgra: np.ndarray  # (height, width, 4) uint8; a view into the receive buffer

    @property
    def timestamp_s(self) -> float:
        return self.timestamp_ns / 1e9

    def rgb(self) -> np.ndarray:
        """(H, W, 3) uint8 RGB copy."""
        return self.bgra[:, :, 2::-1].copy()


def _recv_exact(sock: socket.socket, view: memoryview) -> bool:
    """Fill `view` completely. Returns False on EOF before the first byte, raises on a partial read."""
    got = 0
    while got < len(view):
        n = sock.recv_into(view[got:])
        if n == 0:
            if got == 0:
                return False
            raise ProtocolError("connection closed mid-message")
        got += n
    return True


def _validate(kind: int, width: int, height: int, stride: int, payload_len: int) -> None:
    if kind == KIND_END:
        ok = payload_len == 0
    elif kind == KIND_VIDEO_BGRA8:
        ok = (
            0 < width <= MAX_DIMENSION
            and 0 < height <= MAX_DIMENSION
            and width * 4 <= stride <= MAX_DIMENSION * 4
            and payload_len == stride * height
        )
    else:
        raise ProtocolError(f"unknown message kind {kind}")
    if not ok:
        raise ProtocolError(f"bad geometry {width}x{height} stride {stride} payload {payload_len}")


def read_frames(sock: socket.socket, copy: bool = False) -> Iterator[Frame]:
    """Reads the hello then yields frames until End / EOF.

    By default each Frame's array is a view into a reused buffer that the next frame overwrites;
    pass copy=True (or copy the array yourself) to keep frames around.
    """
    hello = bytearray(HELLO.size)
    if not _recv_exact(sock, memoryview(hello)):
        raise ProtocolError("connection closed before hello")
    magic, version, _flags = HELLO.unpack(hello)
    if magic != MAGIC:
        raise ProtocolError(f"bad magic {magic!r}")
    if version != VERSION:
        raise ProtocolError(f"unsupported protocol version {version}")

    header = bytearray(HEADER.size)
    buf = bytearray()
    while True:
        if not _recv_exact(sock, memoryview(header)):
            return
        kind, width, height, stride, frame_id, ts, payload_len = HEADER.unpack(header)
        _validate(kind, width, height, stride, payload_len)
        if kind == KIND_END:
            return
        if len(buf) < payload_len:
            buf = bytearray(payload_len)
        _recv_exact(sock, memoryview(buf)[:payload_len])
        rows = np.frombuffer(buf, dtype=np.uint8, count=payload_len).reshape(height, stride)
        bgra = rows[:, : width * 4].reshape(height, width, 4)
        if copy:
            bgra = bgra.copy()
        yield Frame(frame_id, ts, width, height, stride, bgra)


class FrameServer:
    """Listens for the Windows sender (which connects to 127.0.0.1:<port> and is forwarded into WSL)."""

    def __init__(self, host: str = "0.0.0.0", port: int = DEFAULT_PORT):
        self._listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self._listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self._listener.bind((host, port))
        self._listener.listen(1)
        self._conn: Optional[socket.socket] = None

    def frames(self, copy: bool = False) -> Iterator[Frame]:
        self._conn, _peer = self._listener.accept()
        self._conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self._conn.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 8 << 20)
        try:
            yield from read_frames(self._conn, copy=copy)
        finally:
            self._conn.close()
            self._conn = None

    def close(self) -> None:
        if self._conn is not None:
            self._conn.close()
        self._listener.close()

    def __enter__(self) -> "FrameServer":
        return self

    def __exit__(self, *exc) -> None:
        self.close()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--host", default="0.0.0.0")
    parser.add_argument("--port", type=int, default=DEFAULT_PORT)
    parser.add_argument("--max-frames", type=int, default=0, help="stop after N frames (0 = until the stream ends)")
    args = parser.parse_args()

    with FrameServer(args.host, args.port) as server:
        print(f"listening on {args.host}:{args.port}", file=sys.stderr)
        n, t0, last = 0, time.monotonic(), None
        for frame in server.frames():
            n += 1
            last = frame
            if args.max_frames and n >= args.max_frames:
                break
        dt = time.monotonic() - t0
        if last is not None:
            print(
                f"{n} frames, {last.width}x{last.height}, {n / dt:.1f} fps, last id {last.frame_id}, "
                f"top-left BGRA {tuple(int(v) for v in last.bgra[0, 0])}",
                file=sys.stderr,
            )
    return 0


if __name__ == "__main__":
    sys.exit(main())
