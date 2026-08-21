//! Video previews: grab one representative frame by shelling out to a
//! thumbnailer that is already on the user's PATH.
//!
//! There is deliberately no native video dependency here (see ROADMAP.md).
//! Linking ffmpeg would drag a C toolchain into every build and break the
//! "painless on Windows" constraint in CLAUDE.md, so instead we look for
//! `ffmpegthumbnailer` (preferred — it is purpose-built for exactly this) or
//! `ffmpeg`, and degrade to a `Metadata` preview when neither is installed.
//! Missing binaries are a normal outcome, never an error.
//!
//! The hard rule in this module: an external process may never stall a
//! preview. Every child is spawned with a deadline, polled while the
//! `CancelToken` is watched, and killed *and reaped* the moment either fires.
//! Nothing here ever calls `Command::output`, which waits unboundedly.

#[cfg(feature = "video")]
pub use imp::render;

#[cfg(feature = "video")]
mod imp {
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use image::imageops::FilterType;
    use image::GenericImageView;

    use crate::{CancelToken, MetaField, Preview, PreviewContent, PreviewError, PreviewOptions};

    /// Wall-clock budget for a frame grab. A hung ffmpeg gets killed here.
    const EXTRACT_TIMEOUT: Duration = Duration::from_secs(5);
    /// ffprobe only reads headers, so it gets a much shorter leash.
    const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
    /// How often we poll the child. Short enough that cancelling a preview
    /// feels instant, long enough not to spin a core while ffmpeg decodes.
    const POLL_INTERVAL: Duration = Duration::from_millis(20);
    /// Seek used when we could not learn the duration. Frame 0 of a video is
    /// very often black (fade-in, slate, leader), so never grab it first.
    const BLIND_SEEK_SECS: f64 = 3.0;

    // ---------------------------------------------------------------- render

    pub fn render(
        path: &Path,
        mime: &str,
        _head: Vec<u8>,
        opts: &PreviewOptions,
        cancel: &CancelToken,
    ) -> Result<Preview, PreviewError> {
        cancel.check()?;

        // Header-only probe: cheap, and its facts are worth showing whether or
        // not the frame grab works out. `None` simply means no ffprobe.
        let probe = probe(path, cancel)?;
        cancel.check()?;

        let Some(tool) = Thumbnailer::find() else {
            // No thumbnailer installed. This is not a failure: describe the
            // file and say how to light frame previews up.
            return Ok(metadata_preview(path, mime, &probe, None));
        };

        match tool.extract(path, &probe, opts, cancel)? {
            Some(frame) => Ok(image_preview(mime, frame, &probe, opts)),
            // The tool is installed but could not decode a frame (truncated
            // download, unsupported codec). If the probe told us something
            // real, that beats nothing; otherwise let the dispatcher fall
            // back to the hexdump, which at least shows the bytes.
            None if probe.is_some() => Ok(metadata_preview(
                path,
                mime,
                &probe,
                Some("frame extraction failed (file may be truncated or use an unsupported codec)"),
            )),
            None => Err(PreviewError::Format(
                "could not extract a video frame".into(),
            )),
        }
    }

    fn image_preview(
        mime: &str,
        frame: image::DynamicImage,
        probe: &Option<Probe>,
        opts: &PreviewOptions,
    ) -> Preview {
        let (fw, fh) = frame.dimensions();
        let max = opts.image_max_dim;

        // The tool was already asked to cap the long edge, so this normally
        // does nothing; it is here so a tool that ignores the request (or
        // upscales) still cannot hand a frontend an oversized buffer.
        let frame = if fw > max || fh > max {
            frame.resize(max, max, FilterType::Triangle)
        } else {
            frame
        };

        // Prefer the probe's dimensions: the frame we hold has already been
        // scaled down by the tool, so it is not the original size. Without
        // ffprobe the scaled frame is the best estimate we have (in practice
        // ffprobe ships alongside ffmpeg, so this is a rare path).
        let (ow, oh) = match probe {
            Some(p) => match (p.width, p.height) {
                (Some(w), Some(h)) if w > 0 && h > 0 => (w, h),
                _ => (fw, fh),
            },
            None => (fw, fh),
        };

        Preview {
            content: PreviewContent::Image {
                image: frame.to_rgba8(),
                original_width: ow,
                original_height: oh,
                format: mime.to_string(),
                fields: probe_fields(probe),
            },
            truncated: ow > max || oh > max,
        }
    }

    fn metadata_preview(
        path: &Path,
        mime: &str,
        probe: &Option<Probe>,
        failure: Option<&str>,
    ) -> Preview {
        let mut fields = vec![MetaField::new("Media type", mime)];
        if let Ok(meta) = std::fs::metadata(path) {
            fields.push(MetaField::new("Size", human_size(meta.len())));
        }
        fields.extend(probe_fields(probe));

        fields.push(match failure {
            Some(why) => MetaField::new("Frame preview", why),
            None => MetaField::new(
                "Frame preview",
                "unavailable — install ffmpegthumbnailer or ffmpeg to show a frame",
            ),
        });

        Preview {
            content: PreviewContent::Metadata {
                fields,
                thumbnail: None,
            },
            // Nothing was cut short by a `PreviewOptions` cap; a missing
            // external tool is not truncation.
            truncated: false,
        }
    }

    /// The probe facts worth showing, in a stable order.
    fn probe_fields(probe: &Option<Probe>) -> Vec<MetaField> {
        let Some(p) = probe else {
            return Vec::new();
        };
        let mut fields = Vec::new();
        if let Some(name) = &p.format_name {
            fields.push(MetaField::new("Container", name));
        }
        if let Some(d) = p.duration {
            fields.push(MetaField::new("Duration", human_duration(d)));
        }
        if let (Some(w), Some(h)) = (p.width, p.height) {
            fields.push(MetaField::new("Resolution", format!("{w}x{h}")));
        }
        if let Some(codec) = &p.codec {
            fields.push(MetaField::new("Video codec", codec));
        }
        if let Some(fps) = p.frame_rate {
            fields.push(MetaField::new("Frame rate", format!("{fps:.3} fps")));
        }
        fields
    }

    // ----------------------------------------------------------- thumbnailer

    enum Thumbnailer {
        /// Purpose-built, picks a sensible frame on its own.
        FfmpegThumbnailer(PathBuf),
        Ffmpeg(PathBuf),
    }

    impl Thumbnailer {
        fn find() -> Option<Self> {
            if let Some(bin) = find_on_path("ffmpegthumbnailer") {
                return Some(Self::FfmpegThumbnailer(bin));
            }
            find_on_path("ffmpeg").map(Self::Ffmpeg)
        }

        /// Write one frame to a temp PNG and decode it. `Ok(None)` means the
        /// tool ran but produced nothing usable — a soft failure.
        fn extract(
            &self,
            path: &Path,
            probe: &Option<Probe>,
            opts: &PreviewOptions,
            cancel: &CancelToken,
        ) -> Result<Option<image::DynamicImage>, PreviewError> {
            let out = TempFile::reserve("frame", "png");
            let input = arg_path(path);
            let max = opts.image_max_dim.max(1);

            match self {
                Self::FfmpegThumbnailer(bin) => {
                    let mut cmd = base_command(bin);
                    // All arguments are flag-value pairs, so there is no
                    // positional slot for a `-`-leading filename to be
                    // mistaken for an option; `arg_path` makes them absolute
                    // regardless. Nothing is ever passed through a shell.
                    cmd.arg("-i")
                        .arg(&input)
                        .arg("-o")
                        .arg(out.path())
                        .arg("-s")
                        .arg(max.to_string())
                        .arg("-c")
                        .arg("png")
                        .arg("-q")
                        .arg("8");
                    run_bounded(cmd, EXTRACT_TIMEOUT, cancel)?;
                }
                Self::Ffmpeg(bin) => {
                    let seek = seek_seconds(probe);
                    run_ffmpeg(bin, &input, out.path(), seek, max, cancel)?;
                    // A short clip can be shorter than the seek we guessed, in
                    // which case ffmpeg writes no frame at all. Retry from the
                    // very start before giving up.
                    if seek > 0.0 && !has_content(out.path()) {
                        run_ffmpeg(bin, &input, out.path(), 0.0, max, cancel)?;
                    }
                }
            }

            cancel.check()?;
            if !has_content(out.path()) {
                return Ok(None);
            }
            // A half-written PNG is a soft failure, not a hard error — the
            // caller still has the probe facts to show. `out` is dropped (and
            // the file deleted) on every path out of here.
            Ok(image::open(out.path()).ok())
        }
    }

    fn run_ffmpeg(
        bin: &Path,
        input: &Path,
        out: &Path,
        seek: f64,
        max: u32,
        cancel: &CancelToken,
    ) -> Result<(), PreviewError> {
        let mut cmd = base_command(bin);
        cmd.arg("-v")
            .arg("error")
            // Never let ffmpeg touch our stdin: a preview pane's stdin is not
            // ours to consume, and ffmpeg's interactive prompts would hang.
            .arg("-nostdin")
            // Seek before -i so ffmpeg jumps by keyframe instead of decoding
            // everything up to that point.
            .arg("-ss")
            .arg(format!("{seek:.3}"))
            .arg("-i")
            .arg(input)
            .arg("-frames:v")
            .arg("1")
            // Cap the long edge without ever upscaling a small video.
            .arg("-vf")
            .arg(format!(
                "scale=w=min(iw\\,{max}):h=min(ih\\,{max}):force_original_aspect_ratio=decrease"
            ))
            .arg("-f")
            .arg("image2")
            .arg("-y")
            .arg(out);
        run_bounded(cmd, EXTRACT_TIMEOUT, cancel)?;
        Ok(())
    }

    /// Where to grab the frame from: ~10% in, which skips fades and slates,
    /// while staying clear of the final moments of a short clip.
    fn seek_seconds(probe: &Option<Probe>) -> f64 {
        let Some(duration) = probe.as_ref().and_then(|p| p.duration) else {
            return BLIND_SEEK_SECS;
        };
        if !duration.is_finite() || duration <= 1.0 {
            return 0.0;
        }
        (duration * 0.10).clamp(0.0, duration - 0.25)
    }

    fn has_content(path: &Path) -> bool {
        std::fs::metadata(path)
            .map(|m| m.len() > 0)
            .unwrap_or(false)
    }

    // ----------------------------------------------------------------- probe

    /// Cheap header facts from ffprobe. Every field is optional: ffprobe
    /// reports what the container happens to carry.
    #[derive(Debug, Default)]
    struct Probe {
        format_name: Option<String>,
        duration: Option<f64>,
        codec: Option<String>,
        width: Option<u32>,
        height: Option<u32>,
        frame_rate: Option<f64>,
    }

    /// `Ok(None)` when ffprobe is not installed or told us nothing.
    fn probe(path: &Path, cancel: &CancelToken) -> Result<Option<Probe>, PreviewError> {
        let Some(bin) = find_on_path("ffprobe") else {
            return Ok(None);
        };
        let out = TempFile::reserve("probe", "txt");
        let Ok(sink) = std::fs::File::create(out.path()) else {
            return Ok(None);
        };

        let mut cmd = base_command(&bin);
        cmd.arg("-v")
            .arg("error")
            .arg("-select_streams")
            .arg("v:0")
            .arg("-show_entries")
            .arg("format=format_name,duration:stream=codec_name,width,height,avg_frame_rate")
            .arg("-of")
            .arg("default=noprint_wrappers=1")
            .arg(arg_path(path))
            // Collect stdout in a file rather than a pipe: a pipe we are not
            // draining while we poll `try_wait` could fill and deadlock the
            // child against our own timeout loop.
            .stdout(Stdio::from(sink));

        if !run_bounded(cmd, PROBE_TIMEOUT, cancel)? {
            return Ok(None);
        }
        let Ok(text) = std::fs::read_to_string(out.path()) else {
            return Ok(None);
        };

        let mut p = Probe::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            // ffprobe writes "N/A" for anything the container omits.
            if value.is_empty() || value == "N/A" {
                continue;
            }
            match key.trim() {
                "format_name" => p.format_name = Some(value.to_string()),
                "duration" => p.duration = value.parse().ok().filter(|d: &f64| *d > 0.0),
                "codec_name" => p.codec = Some(value.to_string()),
                "width" => p.width = value.parse().ok(),
                "height" => p.height = value.parse().ok(),
                "avg_frame_rate" => p.frame_rate = parse_ratio(value),
                _ => {}
            }
        }

        if p.format_name.is_none() && p.duration.is_none() && p.width.is_none() {
            return Ok(None);
        }
        Ok(Some(p))
    }

    /// ffprobe reports frame rates as exact ratios, e.g. `30000/1001`.
    fn parse_ratio(value: &str) -> Option<f64> {
        let (num, den) = value.split_once('/')?;
        let num: f64 = num.trim().parse().ok()?;
        let den: f64 = den.trim().parse().ok()?;
        if den == 0.0 || num <= 0.0 {
            return None;
        }
        Some(num / den)
    }

    // -------------------------------------------------- child process control

    /// Detached from our own stdio in every direction: a preview must not
    /// print to the terminal a frontend owns, nor read its stdin.
    fn base_command(bin: &Path) -> Command {
        let mut cmd = Command::new(bin);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    }

    enum Waited {
        Exited(ExitStatus),
        TimedOut,
    }

    /// Spawn and wait under a deadline. Returns whether the child exited
    /// successfully in time; a timeout is a `false`, not an error, because
    /// the caller always has a graceful fallback.
    fn run_bounded(
        mut cmd: Command,
        timeout: Duration,
        cancel: &CancelToken,
    ) -> Result<bool, PreviewError> {
        cancel.check()?; // boundary: before we spawn anything
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            // The binary was on PATH a moment ago and is not runnable now
            // (removed mid-flight, permissions). Treat it as "no tool".
            Err(_) => return Ok(false),
        };
        let outcome = wait_bounded(&mut child, timeout, cancel)?;
        cancel.check()?; // boundary: after the child returns
        Ok(matches!(outcome, Waited::Exited(status) if status.success()))
    }

    /// Poll `try_wait` until the child exits, the deadline passes, or the
    /// preview is cancelled. The child is killed and reaped in the latter two
    /// cases — this is the guarantee that a hung ffmpeg cannot hang sekio.
    fn wait_bounded(
        child: &mut Child,
        timeout: Duration,
        cancel: &CancelToken,
    ) -> Result<Waited, PreviewError> {
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Ok(Waited::Exited(status)),
                Ok(None) => {}
                Err(e) => {
                    kill_and_reap(child);
                    return Err(PreviewError::Io(e));
                }
            }
            if cancel.is_cancelled() {
                kill_and_reap(child);
                return Err(PreviewError::Cancelled);
            }
            if Instant::now() >= deadline {
                kill_and_reap(child);
                return Ok(Waited::TimedOut);
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Always both: `kill` only sends the signal, `wait` is what stops the
    /// child from lingering as a zombie for the lifetime of the process.
    fn kill_and_reap(child: &mut Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    // ------------------------------------------------------------ path lookup

    /// Resolve `name` against PATH ourselves — a `which` dependency would buy
    /// nothing over `env::split_paths`, which already handles the platform's
    /// separator and Windows quoting rules.
    fn find_on_path(name: &str) -> Option<PathBuf> {
        let path_var = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path_var) {
            // An empty PATH entry means "the current directory" on some
            // shells; resolving relative to cwd is a well-known foot-gun, so
            // skip it rather than run whatever happens to sit there.
            if dir.as_os_str().is_empty() {
                continue;
            }
            for candidate in candidate_names(name) {
                let full = dir.join(candidate);
                if std::fs::metadata(&full)
                    .map(|m| is_executable(&m))
                    .unwrap_or(false)
                {
                    return Some(full);
                }
            }
        }
        None
    }

    #[cfg(windows)]
    fn candidate_names(name: &str) -> Vec<String> {
        // Windows stores the extension on disk, so `ffmpeg` is really
        // `ffmpeg.exe`; try that first, then a bare name for shims.
        vec![format!("{name}.exe"), name.to_string()]
    }

    #[cfg(not(windows))]
    fn candidate_names(name: &str) -> Vec<String> {
        vec![name.to_string()]
    }

    #[cfg(unix)]
    fn is_executable(meta: &std::fs::Metadata) -> bool {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    fn is_executable(meta: &std::fs::Metadata) -> bool {
        meta.is_file()
    }

    /// Absolutise a path before handing it to a child process. ffmpeg has no
    /// `--` end-of-options marker, so this is what keeps a file named
    /// `-i.mp4` from being parsed as a flag: an absolute path can never begin
    /// with `-`. Arguments are always passed as separate argv entries — no
    /// shell, no quoting, no `sh -c`.
    fn arg_path(path: &Path) -> PathBuf {
        if let Ok(abs) = std::path::absolute(path) {
            return abs;
        }
        if path.to_string_lossy().starts_with('-') {
            return Path::new(".").join(path);
        }
        path.to_path_buf()
    }

    // ------------------------------------------------------------- temp files

    /// A path under the OS temp dir that is deleted when this guard drops —
    /// on the success path, on every error path, and on cancellation.
    struct TempFile {
        path: PathBuf,
    }

    impl TempFile {
        /// Reserve a unique name. The file itself is created by the child
        /// process (or not at all); the guard cleans up either way. The pid
        /// separates concurrent sekio processes and the counter separates
        /// concurrent previews inside one — no randomness needed.
        fn reserve(tag: &str, ext: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let name = format!("sekio-{tag}-{}-{n}.{ext}", std::process::id());
            Self {
                path: std::env::temp_dir().join(name),
            }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            // Best-effort: a temp file we cannot remove must not panic a
            // preview, and the OS will reap it eventually.
            let _ = std::fs::remove_file(&self.path);
        }
    }

    // ---------------------------------------------------------- formatting

    fn human_size(bytes: u64) -> String {
        const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
        let mut value = bytes as f64;
        let mut unit = 0;
        while value >= 1024.0 && unit + 1 < UNITS.len() {
            value /= 1024.0;
            unit += 1;
        }
        if unit == 0 {
            format!("{bytes} B")
        } else {
            format!("{value:.1} {} ({bytes} bytes)", UNITS[unit])
        }
    }

    fn human_duration(secs: f64) -> String {
        if !secs.is_finite() || secs < 0.0 {
            return "unknown".to_string();
        }
        let total = secs.round() as u64;
        let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
        if h > 0 {
            format!("{h}:{m:02}:{s:02}")
        } else {
            format!("{m}:{s:02}")
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn finds_a_binary_that_exists_on_path() {
            // Pick something guaranteed by the platform rather than assuming
            // ffmpeg is installed.
            let known = if cfg!(windows) { "cmd" } else { "sh" };
            match find_on_path(known) {
                Some(found) => assert!(found.is_absolute() || found.exists()),
                // A PATH without a shell is exotic but not our bug to fail on.
                None => eprintln!("skipping: {known} not on PATH"),
            }
        }

        #[test]
        fn returns_none_for_a_nonsense_binary() {
            assert!(find_on_path("sekio-definitely-not-a-real-binary-xyzzy").is_none());
        }

        #[test]
        fn empty_path_entries_are_skipped() {
            // Nothing named like this exists anywhere, so an empty entry must
            // not resolve it out of the current directory.
            assert!(find_on_path("").is_none());
        }

        #[test]
        fn temp_guard_removes_its_file_on_drop() {
            let recorded = {
                let tmp = TempFile::reserve("guardtest", "bin");
                std::fs::write(tmp.path(), b"scratch").expect("write temp file");
                assert!(tmp.path().exists(), "file should exist while guard lives");
                tmp.path().to_path_buf()
            };
            assert!(
                !recorded.exists(),
                "TempFile::drop must delete {}",
                recorded.display()
            );
        }

        #[test]
        fn temp_names_are_unique() {
            let a = TempFile::reserve("uniq", "png");
            let b = TempFile::reserve("uniq", "png");
            assert_ne!(a.path(), b.path());
            assert!(a.path().starts_with(std::env::temp_dir()));
        }

        #[test]
        fn arg_path_never_starts_with_a_dash() {
            let arg = arg_path(Path::new("-i.mp4"));
            assert!(!arg.to_string_lossy().starts_with('-'), "{arg:?}");
        }

        #[test]
        fn seek_skips_the_opening_frames_but_stays_in_range() {
            let long = Some(Probe {
                duration: Some(100.0),
                ..Probe::default()
            });
            assert!((seek_seconds(&long) - 10.0).abs() < 1e-9);

            let short = Some(Probe {
                duration: Some(0.5),
                ..Probe::default()
            });
            assert_eq!(seek_seconds(&short), 0.0);

            // No probe at all: a fixed few seconds in, never frame 0.
            assert_eq!(seek_seconds(&None), BLIND_SEEK_SECS);
        }

        /// The core promise of this module: a child that never exits is killed
        /// at the deadline instead of hanging the preview. Uses `sleep`
        /// directly (no shell) and skips if the platform has no such binary.
        #[test]
        #[cfg(unix)]
        fn a_hung_child_is_killed_at_the_deadline() {
            let Some(sleeper) = find_on_path("sleep") else {
                eprintln!("skipping: no sleep binary");
                return;
            };
            let mut cmd = base_command(&sleeper);
            cmd.arg("30");

            let started = Instant::now();
            let ran = run_bounded(cmd, Duration::from_millis(300), &CancelToken::new());
            let elapsed = started.elapsed();

            assert!(matches!(ran, Ok(false)), "timeout must not be an error");
            assert!(
                elapsed < Duration::from_secs(5),
                "waited {elapsed:?} on a 300ms deadline — the child was not killed"
            );
        }

        /// Cancellation mid-wait must kill the child and surface `Cancelled`.
        #[test]
        #[cfg(unix)]
        fn cancellation_mid_wait_kills_the_child() {
            let Some(sleeper) = find_on_path("sleep") else {
                eprintln!("skipping: no sleep binary");
                return;
            };
            let mut cmd = base_command(&sleeper);
            cmd.arg("30");

            let cancel = CancelToken::new();
            let flag = cancel.clone();
            let canceller = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                flag.cancel();
            });

            let started = Instant::now();
            // A deadline far longer than the cancellation, so only the token
            // can be what ended this wait.
            let result = run_bounded(cmd, Duration::from_secs(30), &cancel);
            let elapsed = started.elapsed();
            let _ = canceller.join();

            assert!(matches!(result, Err(PreviewError::Cancelled)));
            assert!(
                elapsed < Duration::from_secs(5),
                "waited {elapsed:?} after cancellation"
            );
        }

        #[test]
        fn parses_ffprobe_frame_rate_ratios() {
            assert!((parse_ratio("30000/1001").unwrap_or(0.0) - 29.97).abs() < 0.01);
            assert_eq!(parse_ratio("0/0"), None);
            assert_eq!(parse_ratio("N/A"), None);
        }

        #[test]
        fn formats_sizes_and_durations() {
            assert_eq!(human_size(512), "512 B");
            assert!(human_size(2 * 1024 * 1024).starts_with("2.0 MiB"));
            assert_eq!(human_duration(65.0), "1:05");
            assert_eq!(human_duration(3725.0), "1:02:05");
        }
    }
}

#[cfg(not(feature = "video"))]
pub fn render(
    _path: &std::path::Path,
    _mime: &str,
    _head: Vec<u8>,
    _opts: &crate::PreviewOptions,
    _cancel: &crate::CancelToken,
) -> Result<crate::Preview, crate::PreviewError> {
    Err(crate::PreviewError::Format(
        "video support not compiled in".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::render;
    use crate::{CancelToken, PreviewContent, PreviewError, PreviewOptions};

    /// Bytes that look like an MP4 to `infer` but decode to nothing. Whatever
    /// tooling the machine happens to have, this must not panic and must not
    /// claim to have produced a frame.
    #[test]
    fn garbage_video_never_panics() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("sekio-video-garbage-{}.mp4", std::process::id()));
        let mut bytes = b"\x00\x00\x00\x18ftypmp42".to_vec();
        bytes.extend(std::iter::repeat_n(0xABu8, 4096));
        std::fs::write(&path, &bytes).expect("write fixture");

        let result = render(
            &path,
            "video/mp4",
            bytes,
            &PreviewOptions::default(),
            &CancelToken::new(),
        );
        let _ = std::fs::remove_file(&path);

        match result {
            Ok(preview) => assert!(
                matches!(preview.content, PreviewContent::Metadata { .. }),
                "an undecodable file must never yield an Image"
            ),
            Err(PreviewError::Cancelled) => panic!("nothing cancelled this preview"),
            Err(_) => {}
        }
    }

    /// An already-cancelled token must surface `Cancelled`, never a swallowed
    /// success or a different error — and never after doing real work.
    #[test]
    fn cancellation_is_reported_not_swallowed() {
        let cancel = CancelToken::new();
        cancel.cancel();
        let result = render(
            std::path::Path::new("does-not-matter.mp4"),
            "video/mp4",
            Vec::new(),
            &PreviewOptions::default(),
            &cancel,
        );
        if cfg!(feature = "video") {
            assert!(matches!(result, Err(PreviewError::Cancelled)));
        } else {
            assert!(matches!(result, Err(PreviewError::Format(_))));
        }
    }
}
