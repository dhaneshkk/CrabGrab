# CrabGrab → WSL bridge

WSL cannot capture the Windows desktop itself: `Windows.Graphics.Capture` and Direct3D 11 only exist on the Windows side, and WSLg only shows Linux apps. The bridge splits the job in two:

```
 Windows                                         WSL2 (Ubuntu)
 ┌──────────────────────────────┐   TCP          ┌──────────────────────────────┐
 │ wsl_bridge_sender.exe        │  127.0.0.1:7878 │ crabgrab-recv  (Rust)        │
 │  CrabGrab capture → BGRA     │ ──────────────▶ │   or crabgrab_bridge.py      │
 │  2-slot queue, drops on lag  │  (forwarded     │   → NumPy (H, W, 4) arrays   │
 └──────────────────────────────┘   into WSL)    └──────────────────────────────┘
```

The sender **connects into WSL** rather than WSL connecting out to Windows. WSL2 forwards Windows' `127.0.0.1` to the distro (`localhostForwarding`, on by default in NAT mode; native in mirrored mode), so there is no host-IP lookup and no Windows Firewall rule to add.

## 1. Build the sender (Windows side)

From PowerShell, with the MSVC Rust toolchain, in a Windows checkout (e.g. `C:\dev\CrabGrab`, not `\\wsl$`):

```powershell
cargo build --release --example wsl_bridge_sender --features bitmap
# -> target\release\examples\wsl_bridge_sender.exe
```

Or cross-compile from WSL (`rustup target add x86_64-pc-windows-gnu`, `sudo apt install mingw-w64`, then `cargo build --release --example wsl_bridge_sender --features bitmap --target x86_64-pc-windows-gnu`).

## 2. Run the receiver (WSL side)

Rust (std only, no dependencies):

```bash
cd bridge/receiver
cargo build --release
./target/release/crabgrab-recv                         # prints resolution / fps / MB/s
./target/release/crabgrab-recv --save-every 30 --out frames   # also dumps every 30th frame as PPM
```

Python / NumPy:

```python
from crabgrab_bridge import FrameServer      # bridge/python/crabgrab_bridge.py

with FrameServer(port=7878) as server:
    for frame in server.frames():
        bgra = frame.bgra                    # (H, W, 4) uint8, zero-copy view
        rgb = frame.rgb()                    # (H, W, 3) uint8 copy, ready for a model
```

## 3. Start capturing

On Windows: `wsl_bridge_sender.exe --display 0` (or `--window "chrome"`, `--list` to see targets). It retries the connection for 30 s, so either side can start first.

Or launch everything from WSL in one command via interop:

```bash
./target/release/crabgrab-recv --exec /mnt/c/dev/CrabGrab/target/release/examples/wsl_bridge_sender.exe -- --display 0 --max-fps 15
```

## Sender options

| Option | Default | |
|---|---|---|
| `--addr HOST:PORT` | `127.0.0.1:7878` | receiver address |
| `--display N` | `0` | display index |
| `--window TEXT` | | first window whose title contains TEXT |
| `--max-fps F` | `30` | cap; skipped frames never leave the GPU (`0` = uncapped) |
| `--seconds S` | until receiver disconnects | |
| `--list` | | list displays and windows |

## Testing without Windows

`crabgrab-fake-sender` speaks the same protocol with synthetic frames (frame *n*'s top-left pixel is B = n & 0xff, G = n >> 8, R = 0x7f):

```bash
./target/release/crabgrab-recv &
./target/release/crabgrab-fake-sender --size 1920x1080 --frames 300 --fps 60
```

## Bandwidth

Frames are raw BGRA: 1080p is ~8.3 MB per frame, so 30 fps ≈ 250 MB/s over the WSL virtual NIC. Loopback inside Linux sustains that comfortably; across the Windows↔WSL boundary, expect the queue to start dropping above roughly 30–60 fps at 1080p depending on the machine. Lower `--max-fps`, or capture a window instead of the full display, if the receiver reports sender drops.

## Troubleshooting

- **`could not connect to 127.0.0.1:7878`**: the receiver isn't listening, or localhost forwarding is off. Check `%UserProfile%\.wslconfig` doesn't set `localhostForwarding=false`; with `networkingMode=mirrored` it works as-is. As a fallback, run `hostname -I` in WSL and pass that address with `--addr`.
- **Black frames**: capture from an interactive desktop session, not over RDP or from a service.
- **`sender-dropped` climbing**: the link is slower than capture; see Bandwidth above.

## Protocol

Defined once in [`receiver/src/protocol.rs`](receiver/src/protocol.rs) and compiled into both the sender (via `#[path]`) and receiver. Little-endian; an 8-byte hello (`"CGRB"`, version `1`, flags) followed by 40-byte headers (`kind, width, height, stride, frame_id, timestamp_ns, payload_len`) each followed by the payload. Receivers validate every header (dimensions ≤ 16384, `stride ≥ width·4`, `payload_len = stride·height`) before allocating.
