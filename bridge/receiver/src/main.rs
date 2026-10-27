//! `crabgrab-recv` - receives frames from the CrabGrab bridge sender running on Windows.
//!
//! Run inside WSL:
//!   crabgrab-recv                                    # listen on 0.0.0.0:7878, print stats
//!   crabgrab-recv --save-every 30 --out frames       # also write every 30th frame as PPM
//!   crabgrab-recv --exec /mnt/c/path/wsl_bridge_sender.exe -- --display 0
//!                                                    # listen, then launch the Windows sender

use std::{
    fs,
    io::Write,
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command},
    time::{Duration, Instant},
};

use crabgrab_bridge::{protocol::DEFAULT_PORT, Frame, FrameReader};

struct Args {
    listen: String,
    save_every: u64,
    out_dir: PathBuf,
    keep_listening: bool,
    exec: Option<String>,
    exec_args: Vec<String>,
}

fn usage() -> ! {
    eprintln!(
        "usage: crabgrab-recv [--listen ADDR] [--save-every N] [--out DIR] [--keep-listening]\n\
         \x20                    [--exec SENDER_EXE [-- SENDER_ARGS...]]\n\
         defaults: --listen 0.0.0.0:{} --out frames",
        DEFAULT_PORT
    );
    std::process::exit(2)
}

fn parse_args() -> Args {
    let mut args = Args {
        listen: format!("0.0.0.0:{}", DEFAULT_PORT),
        save_every: 0,
        out_dir: PathBuf::from("frames"),
        keep_listening: false,
        exec: None,
        exec_args: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--listen" => args.listen = it.next().unwrap_or_else(|| usage()),
            "--save-every" => args.save_every = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "--out" => args.out_dir = PathBuf::from(it.next().unwrap_or_else(|| usage())),
            "--keep-listening" => args.keep_listening = true,
            "--exec" => args.exec = Some(it.next().unwrap_or_else(|| usage())),
            "--" => {
                args.exec_args = it.by_ref().collect();
            }
            "-h" | "--help" => usage(),
            other => {
                eprintln!("unknown argument: {}", other);
                usage()
            }
        }
    }
    args
}

fn write_ppm(path: &PathBuf, frame: &Frame<'_>) -> std::io::Result<()> {
    let mut file = std::io::BufWriter::new(fs::File::create(path)?);
    write!(file, "P6\n{} {}\n255\n", frame.header.width, frame.header.height)?;
    file.write_all(&frame.to_rgb8())?;
    file.flush()
}

fn serve_one(listener: &TcpListener, args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let (stream, peer) = listener.accept()?;
    stream.set_nodelay(true)?;
    eprintln!("sender connected from {}", peer);

    let mut reader = FrameReader::new(stream)?;
    let started = Instant::now();
    let mut window_start = Instant::now();
    let (mut frames, mut bytes, mut total_frames, mut last_id) = (0u64, 0u64, 0u64, None::<u64>);
    let mut skipped_ids = 0u64;

    while let Some(frame) = reader.next_frame()? {
        let h = frame.header;
        // Frame ids are contiguous on the sender; gaps are frames it dropped under backpressure
        if let Some(prev) = last_id {
            skipped_ids += h.frame_id.saturating_sub(prev + 1);
        }
        last_id = Some(h.frame_id);
        frames += 1;
        total_frames += 1;
        bytes += h.payload_len;

        if args.save_every > 0 && total_frames % args.save_every == 1 {
            fs::create_dir_all(&args.out_dir)?;
            let path = args.out_dir.join(format!("frame_{:08}.ppm", h.frame_id));
            write_ppm(&path, &frame)?;
        }

        let elapsed = window_start.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let secs = elapsed.as_secs_f64();
            eprintln!(
                "{}x{}  {:5.1} fps  {:7.1} MB/s  frame {}  t={:.3}s  sender-dropped {}",
                h.width,
                h.height,
                frames as f64 / secs,
                bytes as f64 / secs / 1e6,
                h.frame_id,
                h.timestamp_ns as f64 / 1e9,
                skipped_ids
            );
            frames = 0;
            bytes = 0;
            window_start = Instant::now();
        }
    }
    eprintln!(
        "stream ended: {} frames in {:.1}s ({} dropped by sender)",
        total_frames,
        started.elapsed().as_secs_f64(),
        skipped_ids
    );
    Ok(())
}

fn main() {
    let args = parse_args();
    let listener = TcpListener::bind(&args.listen).unwrap_or_else(|e| {
        eprintln!("failed to listen on {}: {}", args.listen, e);
        std::process::exit(1)
    });
    eprintln!("listening on {}", args.listen);

    // Optionally start the Windows sender through WSL interop once we are listening
    let mut child: Option<Child> = args.exec.as_ref().map(|exe| {
        Command::new(exe).args(&args.exec_args).spawn().unwrap_or_else(|e| {
            eprintln!("failed to launch {}: {}", exe, e);
            std::process::exit(1)
        })
    });

    loop {
        if let Err(e) = serve_one(&listener, &args) {
            eprintln!("stream error: {}", e);
        }
        if !args.keep_listening {
            break;
        }
    }

    if let Some(child) = child.as_mut() {
        let _ = child.wait();
    }
}
