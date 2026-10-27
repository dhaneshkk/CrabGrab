//! `crabgrab-fake-sender` - sends synthetic frames using the bridge protocol, so the WSL side
//! (Rust receiver, Python client, your own pipeline) can be developed and tested without Windows.
//!
//!   crabgrab-fake-sender [--addr 127.0.0.1:7878] [--size 640x360] [--frames 120] [--fps 30]
//!
//! Frame n is a gradient whose top-left pixel encodes n: B = n & 0xff, G = (n >> 8) & 0xff, R = 0x7f.

use std::{
    io::{BufWriter, Write},
    net::TcpStream,
    thread,
    time::{Duration, Instant},
};

use crabgrab_bridge::protocol::{write_hello, write_message, FrameHeader, DEFAULT_PORT};

fn main() {
    let mut addr = format!("127.0.0.1:{}", DEFAULT_PORT);
    let (mut width, mut height, mut count, mut fps) = (640u32, 360u32, 120u64, 30.0f64);
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = || it.next().expect("missing value");
        match arg.as_str() {
            "--addr" => addr = val(),
            "--size" => {
                let v = val();
                let (w, h) = v.split_once('x').expect("--size WxH");
                width = w.parse().expect("width");
                height = h.parse().expect("height");
            }
            "--frames" => count = val().parse().expect("frames"),
            "--fps" => fps = val().parse().expect("fps"),
            other => panic!("unknown argument {}", other),
        }
    }

    let stream = TcpStream::connect(&addr).unwrap_or_else(|e| panic!("connect {}: {}", addr, e));
    stream.set_nodelay(true).ok();
    let mut out = BufWriter::with_capacity(1 << 20, stream);
    write_hello(&mut out).unwrap();

    let start = Instant::now();
    let period = Duration::from_secs_f64(1.0 / fps.max(0.001));
    let mut buf = vec![0u8; width as usize * height as usize * 4];
    for n in 0..count {
        for y in 0..height as usize {
            for x in 0..width as usize {
                let i = (y * width as usize + x) * 4;
                buf[i] = (x * 255 / width as usize) as u8;
                buf[i + 1] = (y * 255 / height as usize) as u8;
                buf[i + 2] = n as u8;
                buf[i + 3] = 255;
            }
        }
        buf[0] = n as u8;
        buf[1] = (n >> 8) as u8;
        buf[2] = 0x7f;
        let ts = start.elapsed().as_nanos() as u64;
        write_message(&mut out, &FrameHeader::video(width, height, n, ts), &buf).unwrap();
        out.flush().unwrap();
        let next = period * (n as u32 + 1);
        if let Some(wait) = next.checked_sub(start.elapsed()) {
            thread::sleep(wait);
        }
    }
    write_message(&mut out, &FrameHeader::end(), &[]).unwrap();
    out.flush().unwrap();
    eprintln!("sent {} frames", count);
}
