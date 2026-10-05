use std::{io::Write, process::{Child, Command, Stdio}};

use scap::{capturer::{self, Capturer}, frame::{Frame, YUVFrame}};

fn nvenc_available() -> bool {
    Command::new("ffmpeg")
        .args([
            "-hide_banner", "-loglevel", "error",
            "-f", "lavfi", "-i", "nullsrc=s=256x256:d=0.1",
            "-frames:v", "1",
            "-c:v", "h264_nvenc",
            "-f", "null", "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}


fn spawn_encoder(width: u32, height: u32, fps: u32, out: &str) -> Child {
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-y",
        "-f", "rawvideo",
        "-pix_fmt", "bgra",
        "-s", &format!("{width}x{height}"),
        "-r", &fps.to_string(),
        "-i", "-",
    ]);

    if nvenc_available() {
        cmd.args([
            "-c:v", "h264_nvenc",
            "-preset", "p1",      // p1 = fastest ... p7 = best quality
            "-tune", "ll",        // low latency ("ull" for ultra low latency)
            "-rc", "vbr",
            "-cq", "26",          // quality target, lower = better/larger
            "-b:v", "0",          // let -cq drive the bitrate
        ]);
    } else {
        cmd.args([
            "-c:v", "libx264",
            "-preset", "ultrafast",
            "-tune", "zerolatency",
            "-crf", "26",
        ]);
    }

    cmd.args(["-pix_fmt", "yuv420p", out])
        .stdin(Stdio::piped())
        .spawn()
        .expect("failed to start ffmpeg")
}

fn main() {
    if !scap::is_supported() {
        eprintln!("Unsuppored platform. Exiting.");
        return;
    }

    // Check if the program has permission to record the screen on MacOS
    if !scap::has_permission() {
        println!("Permission not granted. Requesting permission...");
        if !scap::request_permission() {
            println!("Permission denied");
            return;
        }
    }

    let options = capturer::Options {
        fps: 60,
        target: None, // None captures the primary display
        show_cursor: true,
        show_highlight: true,
        excluded_targets: None,
        output_type: scap::frame::FrameType::BGRAFrame,
        output_resolution: scap::capturer::Resolution::_720p,
        ..Default::default()
    };

    // Create Capturer
    let mut capturer = Capturer::build(options).unwrap();

    // Start Capture
    eprintln!("starting capture");
    capturer.start_capture();

    let (width, height) = loop {
        eprintln!("finding w/h");
        if let Ok(Frame::BGRx(frame)) = capturer.get_next_frame() {
            break (frame.width as u32, frame.height as u32);
        }
    };

    let mut enc = spawn_encoder(width, height, 30, "out.mp4");
    let mut stdin = enc.stdin.take().unwrap();

    loop {
        let frame = capturer.get_next_frame().unwrap();
        if let Frame::BGRx(frame) = frame {
            // for each captured BGRA frame:
            stdin.write_all(&frame.data).unwrap();
        }
    }

    drop(stdin);
    enc.wait().unwrap();
}
