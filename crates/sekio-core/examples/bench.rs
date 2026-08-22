//! Preview latency per format. Speed is the whole point of sekio, so this
//! exists to make regressions visible rather than felt.
//!
//! Run with:  cargo run --release -p sekio-core --example bench [-- path...]
//!
//! With no arguments it generates synthetic fixtures covering each renderer.
//! With paths, it times those files instead — useful for checking a real
//! directory of holiday photos or a pathological log file.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sekio_core::{CancelToken, PreviewContent, PreviewOptions, Previewer};

const RUNS: u32 = 20;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let previewer = Previewer::new();
    let opts = PreviewOptions::default();

    let (paths, _keep) = if args.is_empty() {
        let fixtures = Fixtures::generate();
        (fixtures.paths.clone(), Some(fixtures))
    } else {
        (args.iter().map(PathBuf::from).collect(), None)
    };

    println!(
        "{:<22} {:>10} {:>10} {:>10}   content",
        "file", "median", "min", "max"
    );
    println!("{}", "-".repeat(70));

    for path in &paths {
        let mut timings = Vec::with_capacity(RUNS as usize);
        let mut label = String::from("(error)");

        for _ in 0..RUNS {
            let start = Instant::now();
            match previewer.preview(path, &opts, &CancelToken::new()) {
                Ok(preview) => {
                    timings.push(start.elapsed());
                    label = describe(&preview.content);
                }
                Err(e) => {
                    label = format!("({e})");
                    break;
                }
            }
        }

        if timings.is_empty() {
            println!(
                "{:<22} {:>10} {:>10} {:>10}   {label}",
                name(path),
                "-",
                "-",
                "-"
            );
            continue;
        }

        timings.sort();
        println!(
            "{:<22} {:>10} {:>10} {:>10}   {label}",
            name(path),
            ms(timings[timings.len() / 2]),
            ms(timings[0]),
            ms(timings[timings.len() - 1]),
        );
    }
}

fn name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn ms(d: Duration) -> String {
    format!("{:.2}ms", d.as_secs_f64() * 1000.0)
}

fn describe(content: &PreviewContent) -> String {
    match content {
        PreviewContent::Text { lines, language } => {
            format!("Text/{language} ({} lines)", lines.len())
        }
        PreviewContent::Image {
            original_width,
            original_height,
            format,
            ..
        } => format!("Image/{format} ({original_width}x{original_height})"),
        PreviewContent::Listing { entries } => format!("Listing ({} entries)", entries.len()),
        PreviewContent::Metadata { fields, .. } => format!("Metadata ({} fields)", fields.len()),
        PreviewContent::Table { columns, rows, .. } => {
            format!("Table ({} rows x {} columns)", rows.len(), columns.len())
        }
        PreviewContent::HexDump { mime, .. } => {
            format!("HexDump/{}", mime.as_deref().unwrap_or("binary"))
        }
    }
}

/// Synthetic inputs, removed when the run finishes.
struct Fixtures {
    dir: PathBuf,
    paths: Vec<PathBuf>,
}

impl Fixtures {
    fn generate() -> Self {
        let dir = std::env::temp_dir().join(format!("sekio-bench-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create bench dir");
        let mut paths = Vec::new();

        let mut write = |name: &str, bytes: &[u8]| {
            let path = dir.join(name);
            std::fs::write(&path, bytes).expect("write fixture");
            paths.push(path);
        };

        // A realistic source file, repeated to a few hundred lines.
        let code = "fn compute(x: u32) -> u32 {\n    let y = x * 2;\n    y + 1\n}\n".repeat(120);
        write("source.rs", code.as_bytes());

        let md = "# Heading\n\nSome *emphasis* and `code`.\n\n- one\n- two\n\n> quote\n".repeat(60);
        write("doc.md", md.as_bytes());

        let log = "2026-08-21T10:00:00Z INFO  request served in 12ms\n".repeat(2000);
        write("app.log", log.as_bytes());

        // 1920x1080 is a common real-world photo size.
        let mut png = Vec::new();
        let img = image::RgbaImage::from_fn(1920, 1080, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
        });
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encode png");
        write("photo.png", &png);

        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="800" height="600">{}</svg>"#,
            (0..200)
                .map(|i| format!(
                    r##"<circle cx="{}" cy="{}" r="20" fill="#3a7"/>"##,
                    i * 7 % 800,
                    i * 11 % 600
                ))
                .collect::<String>()
        );
        write("drawing.svg", svg.as_bytes());

        // 8 MB of incompressible-looking binary.
        let blob: Vec<u8> = (0..8 * 1024 * 1024u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 16) as u8)
            .collect();
        write("blob.bin", &blob);

        paths.push(dir.clone());

        Self { dir, paths }
    }
}

impl Drop for Fixtures {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
