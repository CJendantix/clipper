// TODO: modularize ffmpeg arguments to create a typed api for the rest of the program

use std::{
    io::Write, path::Path, process::{Child, Command, Stdio}, sync::{
        Arc, atomic::{AtomicBool, Ordering}
    }
};

use scap::{capturer::{self, Capturer}, frame::Frame};

fn nvenc_available() -> bool {
    Command::new("ffmpeg")
        .args([
            "-hide_banner", "-loglevel", "error",
            "-f", "lavfi", "-i", "nullsrc=s=256x256:d=0.1",
            "-frames:v", "1", "-c:v", "h264_nvenc", "-f", "null", "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn spawn_encoder(width: u32, height: u32, fps: u32, dir: &str, keep_secs: u32) -> Child {
    std::fs::create_dir_all(dir).unwrap();
    let seg_secs = 2;

    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-y",
        "-f", "rawvideo",
        "-pix_fmt", "bgr0",
        "-s", &format!("{width}x{height}"),
        "-use_wallclock_as_timestamps", "1",
        "-i", "-",
        "-vf", "pad=ceil(iw/2)*2:ceil(ih/2)*2",
        "-fps_mode", "cfr",
        "-r", &fps.to_string(),
    ]);

    if nvenc_available() {
        cmd.args([
            "-c:v", "h264_nvenc", "-preset", "p1", "-tune", "ll",
            "-rc", "vbr", "-cq", "26", "-b:v", "0",
        ]);
    } else {
        cmd.args([
            "-c:v", "libx264", "-preset", "ultrafast",
            "-tune", "zerolatency", "-crf", "26",
        ]);
    }

    cmd.args([
        "-pix_fmt", "yuv420p",
        // A keyframe at every segment boundary, so segments can be cut cleanly
        "-g", &(fps * seg_secs).to_string(),
        "-f", "hls",
        "-hls_time", &seg_secs.to_string(),
        "-hls_list_size", &(keep_secs / seg_secs).to_string(), // segments kept in the playlist
        "-hls_flags", "delete_segments",                       // delete ones that fall off
        "-hls_segment_type", "mpegts",                         // survives a crash, unlike mp4
        "-hls_segment_filename", &format!("{dir}/seg_%05d.ts"),
        &format!("{dir}/live.m3u8"),
    ])
    .stdin(Stdio::piped())
    .spawn()
    .expect("failed to start ffmpeg")
}

fn hls_to_mp4(dir: &str, out: &str) -> std::io::Result<()> {
    let playlist = format!("{dir}/live.m3u8");
    if !Path::new(&playlist).exists() {
        eprintln!("no playlist found, nothing to convert");
        return Ok(());
    }

    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-loglevel", "warning",
            "-i", &playlist,
            "-c", "copy",                 // no re-encode, near instant
            "-movflags", "+faststart",    // put the index at the front for easy playback
            out,
        ])
        .status()?;

    if !status.success() {
        eprintln!("remux failed, segments left in {dir}");
    }
    Ok(())
}

fn main() {
    if !scap::is_supported() {
        eprintln!("Unsupported platform.");
        return;
    }
    if !scap::has_permission() && !scap::request_permission() {
        eprintln!("Permission denied");
        return;
    }

    let fps = 30;
    let options = capturer::Options {
        fps,
        target: None,
        show_cursor: true,
        show_highlight: true,
        excluded_targets: None,
        output_type: scap::frame::FrameType::BGRAFrame,
        output_resolution: scap::capturer::Resolution::_720p,
        ..Default::default()
    };

    let mut capturer = Capturer::build(options).unwrap();
    capturer.start_capture();

    // Ctrl-C sets a flag instead of killing us mid-write
    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        ctrlc::set_handler(move || r.store(false, Ordering::SeqCst)).unwrap();
    }

    // Wait for the first frame to learn the size
    let first = loop {
        match capturer.get_next_frame() {
            Ok(Frame::BGRx(f)) => break f,
            Ok(other) => eprintln!("unexpected frame type: {:?}", std::mem::discriminant(&other)),
            Err(e) => eprintln!("{e}"),
        }
    };

    let mut enc = spawn_encoder(first.width as u32, first.height as u32, fps, "out", 60);
    let mut stdin = enc.stdin.take().unwrap();
    stdin.write_all(&first.data).unwrap();

    while running.load(Ordering::SeqCst) {
        if let Ok(Frame::BGRx(f)) = capturer.get_next_frame() {
            if stdin.write_all(&f.data).is_err() {
                eprintln!("ffmpeg exited early, check its stderr output");
                break;
            }
        }
    }

    capturer.stop_capture();
    drop(stdin);        // EOF tells ffmpeg to finalize the MP4
    enc.wait().unwrap();

    let _ = hls_to_mp4("out", "out.mp4");
}