// TODO: modularize ffmpeg arguments to create a typed api for the rest of the program

use std::{
    io::Write,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use scap::{
    capturer::{self, Capturer},
    frame::Frame,
};

fn nvenc_available() -> bool {
    Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "nullsrc=s=256x256:d=0.1",
            "-frames:v",
            "1",
            "-c:v",
            "h264_nvenc",
            "-f",
            "null",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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
            "-loglevel",
            "warning",
            "-i",
            &playlist,
            "-c",
            "copy", // no re-encode, near instant
            "-movflags",
            "+faststart", // put the index at the front for easy playback
            out,
        ])
        .status()?;

    if !status.success() {
        eprintln!("remux failed, segments left in {dir}");
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Encoder {
    Nvenc,
    X264,
}

impl Encoder {
    /// Probe once, pick the best available
    fn detect() -> Self {
        if nvenc_available() {
            Self::Nvenc
        } else {
            Self::X264
        }
    }

    fn args(self, quality: u8) -> Vec<String> {
        let q = quality.to_string();
        match self {
            Self::Nvenc => [
                "-c:v",
                "h264_nvenc",
                "-preset",
                "p1",
                "-tune",
                "ll",
                "-rc",
                "vbr",
                "-cq",
                &q,
                "-b:v",
                "0",
            ],
            Self::X264 => [
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-tune",
                "zerolatency",
                "-crf",
                &q,
                "",
                "",
                "",
                "",
            ],
        }
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
    }
}

struct RollingHlsConfig {
    dir: String,
    keep_secs: u32,
    segment_secs: u32,
}

struct EncoderConfig {
    width: u32,
    height: u32,
    fps: u32,
    encoder: Encoder,
    quality: u8,
    output: RollingHlsConfig,
}

impl EncoderConfig {
    fn to_args(&self) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "-y".into(),
            "-f".into(),
            "rawvideo".into(),
            "-pix_fmt".into(),
            "bgr0".into(),
            "-s".into(),
            format!("{}x{}", self.width, self.height),
            "-use_wallclock_as_timestamps".into(),
            "1".into(),
            "-i".into(),
            "-".into(),
            "-vf".into(),
            "pad=ceil(iw/2)*2:ceil(ih/2)*2".into(),
            "-fps_mode".into(),
            "cfr".into(),
            "-r".into(),
            self.fps.to_string(),
        ];
        args.extend(self.encoder.args(self.quality));
        args.extend(["-pix_fmt".into(), "yuv420p".into()]);

        let dir = &self.output.dir;
        let keep_secs = self.output.keep_secs;
        let segment_secs = self.output.segment_secs;
        args.extend([
            "-g".into(),
            (self.fps * segment_secs).to_string(),
            "-f".into(),
            "hls".into(),
            "-hls_time".into(),
            segment_secs.to_string(),
            "-hls_list_size".into(),
            (keep_secs / segment_secs).to_string(),
            "-hls_flags".into(),
            "delete_segments".into(),
            "-hls_segment_type".into(),
            "mpegts".into(),
            "-hls_segment_filename".into(),
            format!("{dir}/seg_%05d.ts"),
            format!("{dir}/live.m3u8"),
        ]);

        args
    }

    fn spawn(&self) -> std::io::Result<Child> {
        std::fs::create_dir_all(&self.output.dir)?;
        Command::new("ffmpeg")
            .args(self.to_args())
            .stdin(Stdio::piped())
            .spawn()
    }
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
            Ok(other) => eprintln!(
                "unexpected frame type: {:?}",
                std::mem::discriminant(&other)
            ),
            Err(e) => eprintln!("{e}"),
        }
    };

    let mut enc = EncoderConfig {
        width: first.width as u32,
        height: first.height as u32,
        fps: 30,
        encoder: Encoder::detect(),
        quality: 26,
        output: RollingHlsConfig {
            dir: "buffer".into(),
            keep_secs: 60,
            segment_secs: 2,
        },
    }
    .spawn()
    .unwrap();

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
    drop(stdin); // EOF tells ffmpeg to finalize the MP4
    enc.wait().unwrap();

    let _ = hls_to_mp4("buffer", "out.mp4");
}
