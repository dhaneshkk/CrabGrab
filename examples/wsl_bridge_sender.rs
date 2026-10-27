//! Windows (or macOS) side of the WSL bridge: captures a display or window with CrabGrab and
//! streams BGRA frames over TCP to a receiver running inside WSL (`bridge/receiver` or
//! `bridge/python/crabgrab_bridge.py`).
//!
//! The sender connects to 127.0.0.1 by default. WSL2 forwards Windows' localhost into the distro
//! (default `localhostForwarding=true` in NAT mode, and natively in mirrored mode), so start the
//! receiver in WSL first, then run this on Windows:
//!
//!   cargo run --release --example wsl_bridge_sender --features bitmap -- [options]
//!
//! Options:
//!   --addr HOST:PORT     receiver address (default 127.0.0.1:7878)
//!   --display N          capture display N (default 0)
//!   --window TEXT        capture the first window whose title contains TEXT (case-insensitive)
//!   --max-fps F          cap the frame rate sent over the bridge (default 30; 0 = uncapped)
//!   --seconds S          stop after S seconds (default: until the receiver disconnects)
//!   --list               list displays and windows, then exit
//!
//! Frames are copied to system memory on the capture thread and handed to a network thread through
//! a 2-slot queue; when the network is slower than capture, frames are dropped (never queued
//! without bound), and the receiver sees the gap in frame ids.

#[path = "../bridge/receiver/src/protocol.rs"]
#[allow(dead_code)]
mod protocol;

use std::{
    io::{BufWriter, Write},
    net::TcpStream,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, TrySendError},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use crabgrab::{feature::bitmap::{FrameBitmap, VideoFrameBitmap}, prelude::*};
use protocol::{FrameHeader, DEFAULT_PORT};

struct Args {
    addr: String,
    display: usize,
    window: Option<String>,
    max_fps: f64,
    seconds: Option<f64>,
    list: bool,
}

fn usage() -> ! {
    eprintln!("usage: wsl_bridge_sender [--addr HOST:PORT] [--display N | --window TEXT] [--max-fps F] [--seconds S] [--list]");
    std::process::exit(2)
}

fn parse_args() -> Args {
    let mut args = Args {
        addr: format!("127.0.0.1:{}", DEFAULT_PORT),
        display: 0,
        window: None,
        max_fps: 30.0,
        seconds: None,
        list: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--addr" => args.addr = it.next().unwrap_or_else(|| usage()),
            "--display" => args.display = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "--window" => args.window = Some(it.next().unwrap_or_else(|| usage())),
            "--max-fps" => args.max_fps = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "--seconds" => args.seconds = Some(it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage())),
            "--list" => args.list = true,
            _ => usage(),
        }
    }
    args
}

enum Packet {
    Frame(FrameHeader, Box<[[u8; 4]]>),
    End,
}

/// Connects to the receiver, retrying for ~30s so the sender can be started first.
fn connect(addr: &str) -> TcpStream {
    for attempt in 0..30 {
        match TcpStream::connect(addr) {
            Ok(stream) => return stream,
            Err(e) if attempt == 0 => eprintln!("waiting for receiver at {} ({})...", addr, e),
            Err(_) => {}
        }
        thread::sleep(Duration::from_secs(1));
    }
    eprintln!("could not connect to {} - is crabgrab-recv running inside WSL?", addr);
    std::process::exit(1)
}

async fn run(args: Args) {
    let token = match CaptureStream::test_access(false) {
        Some(token) => token,
        None => CaptureStream::request_access(false).await.expect("screen capture access was denied"),
    };
    let content = CapturableContent::new(CapturableContentFilter::EVERYTHING_NORMAL)
        .await
        .expect("failed to enumerate capturable content");

    if args.list {
        for (i, display) in content.displays().enumerate() {
            eprintln!("display {}: {:?}", i, display.rect());
        }
        for window in content.windows().filter(|w| !w.title().is_empty()) {
            eprintln!("window: {:?} ({})", window.title(), window.application().name());
        }
        return;
    }

    let pixel_format = CapturePixelFormat::Bgra8888;
    let config = match &args.window {
        Some(needle) => {
            let needle = needle.to_lowercase();
            let window = content
                .windows()
                .find(|w| w.title().to_lowercase().contains(&needle))
                .unwrap_or_else(|| {
                    eprintln!("no window title contains {:?} (try --list)", needle);
                    std::process::exit(1)
                });
            eprintln!("capturing window {:?}", window.title());
            CaptureConfig::with_window(window, pixel_format).expect("window capture config")
        }
        None => {
            let display = content.displays().nth(args.display).unwrap_or_else(|| {
                eprintln!("no display {} (try --list)", args.display);
                std::process::exit(1)
            });
            eprintln!("capturing display {} {:?}", args.display, display.rect());
            CaptureConfig::with_display(display, pixel_format)
        }
    };

    let stream_out = connect(&args.addr);
    stream_out.set_nodelay(true).ok();
    eprintln!("connected to {}", args.addr);

    // Two slots: one frame being written to the socket, one waiting. Anything more is dropped.
    let (tx, rx) = mpsc::sync_channel::<Packet>(2);
    let disconnected = Arc::new(AtomicBool::new(false));
    let net_disconnected = disconnected.clone();

    let net_thread = thread::spawn(move || {
        let mut out = BufWriter::with_capacity(1 << 20, stream_out);
        let result = (|| -> std::io::Result<()> {
            protocol::write_hello(&mut out)?;
            out.flush()?;
            for packet in rx {
                match packet {
                    Packet::Frame(header, pixels) => {
                        protocol::write_message(&mut out, &header, pixels.as_flattened())?;
                        out.flush()?;
                    }
                    Packet::End => {
                        protocol::write_message(&mut out, &FrameHeader::end(), &[])?;
                        out.flush()?;
                        break;
                    }
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            eprintln!("receiver disconnected: {}", e);
        }
        net_disconnected.store(true, Ordering::Release);
    });

    let sent = Arc::new(AtomicU64::new(0));
    let dropped = Arc::new(AtomicU64::new(0));
    let (cb_sent, cb_dropped) = (sent.clone(), dropped.clone());
    let min_interval = if args.max_fps > 0.0 { Duration::from_secs_f64(1.0 / args.max_fps) } else { Duration::ZERO };
    let mut last_sent: Option<Instant> = None;
    let mut seq = 0u64;

    let mut stream = CaptureStream::new(token, config, move |event| match event {
        Ok(StreamEvent::Video(frame)) => {
            let now = Instant::now();
            if last_sent.map_or(false, |t| now - t < min_interval) {
                return; // rate cap: skip before paying for the GPU -> CPU copy
            }
            let bitmap = match frame.get_bitmap() {
                Ok(FrameBitmap::BgraUnorm8x4(bitmap)) => bitmap,
                Ok(_) => {
                    eprintln!("unexpected bitmap format (requested BGRA8)");
                    return;
                }
                Err(e) => {
                    eprintln!("bitmap error: {:?}", e);
                    return;
                }
            };
            last_sent = Some(now);
            // Sequence number of frames *offered* to the bridge, so receiver-side gaps == bridge drops
            let id = seq;
            seq += 1;
            let header = FrameHeader::video(
                bitmap.width as u32,
                bitmap.height as u32,
                id,
                frame.origin_time().as_nanos() as u64,
            );
            match tx.try_send(Packet::Frame(header, bitmap.data)) {
                Ok(()) => {
                    cb_sent.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Full(_)) => {
                    cb_dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => {}
            }
        }
        Ok(StreamEvent::End) => {
            let _ = tx.send(Packet::End);
        }
        Ok(_) => {}
        Err(e) => eprintln!("capture error: {:?}", e),
    })
    .expect("failed to start capture stream");

    let started = Instant::now();
    let mut last_report = Instant::now();
    while !disconnected.load(Ordering::Acquire) {
        if let Some(limit) = args.seconds {
            if started.elapsed().as_secs_f64() >= limit {
                break;
            }
        }
        thread::sleep(Duration::from_millis(100));
        if last_report.elapsed() >= Duration::from_secs(5) {
            eprintln!("sent {} frames, dropped {}", sent.load(Ordering::Relaxed), dropped.load(Ordering::Relaxed));
            last_report = Instant::now();
        }
    }

    // stop() delivers StreamEvent::End exactly once, which queues the End message for the receiver
    let _ = stream.stop();
    drop(stream);
    let _ = net_thread.join();
    eprintln!("done: sent {} frames, dropped {}", sent.load(Ordering::Relaxed), dropped.load(Ordering::Relaxed));
}

fn main() {
    let args = parse_args();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    runtime.block_on(run(args));
}
