//! Audio metadata renderer: tags + technical facts, plus embedded cover art.
//!
//! Metadata only — samples are never decoded, and the container is only
//! *probed* (symphonia caps that at 1 MB from the head plus a few anchor
//! points near the tail), so a two-hour FLAC returns as fast as a jingle.
//! Duration comes from the container's declared duration; if a format doesn't
//! declare one, the field is simply omitted rather than scanned for.
//!
//! Feature-gated in-file: with `audio` off, `render` still exists and returns
//! `PreviewError::Format`, so the dispatcher degrades to a hexdump.

use std::path::Path;

use crate::{CancelToken, Preview, PreviewError, PreviewOptions};

#[cfg(feature = "audio")]
pub fn render(
    path: &Path,
    mime: &str,
    _head: Vec<u8>,
    opts: &PreviewOptions,
    cancel: &CancelToken,
) -> Result<Preview, PreviewError> {
    use symphonia::core::codecs::audio::AudioCodecParameters;
    use symphonia::core::codecs::CodecParameters;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
    use symphonia::core::meta::MetadataOptions;

    use crate::{MetaField, PreviewContent};

    // Hand symphonia the file handle: it buffers and seeks, never slurps.
    let file = std::fs::File::open(path)?;
    let file_size = file.metadata().map(|m| m.len()).ok();
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());

    let mut hint = Hint::new();
    hint.mime_type(mime);
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    // `prebuild_seek_index` defaults to false — leave it there, building an
    // index would walk the whole file.
    let mut reader = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| PreviewError::Format(format!("audio: {e}")))?;

    cancel.check()?;

    // Metadata revisions arrive from two places: readers found *during* the
    // probe (ID3v2 ahead of the stream, ID3v1/APE behind it) are appended to
    // the format reader's log by `Probe::probe` itself, and the container's
    // own tags are pushed by the reader. In v0.6 both therefore surface
    // through `FormatReader::metadata()` — but as a *log* of revisions, and
    // `current()` is the oldest one. Walk the whole log so nothing is missed,
    // letting newer revisions win.
    let mut tags = Tags::default();
    let mut cover: Option<Cover> = None;
    {
        let mut log = reader.metadata();
        loop {
            if let Some(rev) = log.current() {
                // Per-track first so media-level tags override them.
                for per_track in &rev.per_track {
                    harvest(&per_track.metadata, &mut tags, &mut cover);
                }
                harvest(&rev.media, &mut tags, &mut cover);
            }
            if log.pop().is_none() {
                break;
            }
        }
    }
    cancel.check()?;

    // Technical facts come from the default audio track and the media info.
    let track = reader
        .default_track(TrackType::Audio)
        .or_else(|| reader.first_track(TrackType::Audio));
    let audio: Option<&AudioCodecParameters> = track.and_then(|t| match &t.codec_params {
        Some(CodecParameters::Audio(p)) => Some(p),
        _ => None,
    });

    let duration_secs = {
        let info = reader.media_info();
        match (info.time_base, info.duration) {
            (Some(tb), Some(d)) => tb.calc_duration(d).map(|t| t.as_secs_f64()),
            _ => None,
        }
    };

    let mut fields: Vec<MetaField> = Vec::new();
    let mut push = |key: &str, value: Option<String>| {
        if let Some(v) = value {
            if !v.trim().is_empty() {
                fields.push(MetaField::new(key, v.trim()));
            }
        }
    };

    let track_display = tags.track_display();

    // Reading order: what a human looks for first, then the technical detail.
    push("Title", tags.title);
    push("Artist", tags.artist);
    push("Album", tags.album);
    push("Album Artist", tags.album_artist);
    push("Track", track_display);
    push("Date", tags.date.map(|(_, v)| v));
    push("Genre", tags.genre);

    push("Duration", duration_secs.map(format_duration));
    push(
        "Codec",
        audio.and_then(|p| codec_name(p.codec)).map(String::from),
    );
    push(
        "Container",
        Some(reader.format_info().long_name.to_string()),
    );
    push(
        "Sample Rate",
        audio.and_then(|p| p.sample_rate).map(format_sample_rate),
    );
    push(
        "Channels",
        audio
            .and_then(|p| p.channels.as_ref())
            .map(|c| format_channels(c.count())),
    );
    push(
        "Bit Depth",
        audio
            .and_then(|p| p.bits_per_sample.or(p.bits_per_coded_sample))
            .map(|b| format!("{b}-bit")),
    );
    push("Bitrate", overall_bitrate(file_size, duration_secs));
    push("Size", file_size.map(human_size));
    push("Type", Some(mime.to_string()));

    // A cover that won't decode is a missing thumbnail, never a failed
    // preview — but cancellation still propagates.
    cancel.check()?;
    let thumbnail = cover.and_then(|c| decode_cover(&c.data, opts.image_max_dim));
    cancel.check()?;

    // The only cap that can bite here is the thumbnail downscale.
    let truncated = thumbnail
        .as_ref()
        .is_some_and(|t| t.width() == opts.image_max_dim || t.height() == opts.image_max_dim);

    Ok(Preview {
        content: PreviewContent::Metadata { fields, thumbnail },
        truncated,
    })
}

#[cfg(not(feature = "audio"))]
pub fn render(
    _path: &Path,
    _mime: &str,
    _head: Vec<u8>,
    _opts: &PreviewOptions,
    _cancel: &CancelToken,
) -> Result<Preview, PreviewError> {
    Err(PreviewError::Format("audio support not compiled in".into()))
}

// ---------------------------------------------------------------------------
// Tag harvesting
// ---------------------------------------------------------------------------

/// Tags we care about, accumulated across every metadata revision.
#[cfg(feature = "audio")]
#[derive(Default)]
struct Tags {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    album_artist: Option<String>,
    track_no: Option<u64>,
    track_total: Option<u64>,
    genre: Option<String>,
    /// Ranked so a release date beats a fallback like the tagging year.
    date: Option<(u8, String)>,
}

#[cfg(feature = "audio")]
impl Tags {
    fn set_date(&mut self, rank: u8, value: String) {
        if self.date.as_ref().is_none_or(|(r, _)| rank >= *r) {
            self.date = Some((rank, value));
        }
    }

    fn track_display(&self) -> Option<String> {
        match (self.track_no, self.track_total) {
            (Some(n), Some(t)) => Some(format!("{n}/{t}")),
            (Some(n), None) => Some(n.to_string()),
            _ => None,
        }
    }
}

/// Cover art candidate, ranked so a real front cover beats a band logo or icon.
#[cfg(feature = "audio")]
struct Cover {
    rank: u8,
    data: Vec<u8>,
}

#[cfg(feature = "audio")]
fn harvest(
    container: &symphonia::core::meta::MetadataContainer,
    tags: &mut Tags,
    cover: &mut Option<Cover>,
) {
    use symphonia::core::meta::{StandardTag, StandardVisualKey};

    for tag in &container.tags {
        match &tag.std {
            Some(StandardTag::TrackTitle(v)) => tags.title = Some(v.to_string()),
            Some(StandardTag::Artist(v)) => tags.artist = Some(v.to_string()),
            Some(StandardTag::Album(v)) => tags.album = Some(v.to_string()),
            Some(StandardTag::AlbumArtist(v)) => tags.album_artist = Some(v.to_string()),
            Some(StandardTag::Genre(v)) => tags.genre = Some(v.to_string()),
            Some(StandardTag::TrackNumber(n)) => tags.track_no = Some(*n),
            Some(StandardTag::TrackTotal(n)) => tags.track_total = Some(*n),
            Some(StandardTag::ReleaseDate(v)) => tags.set_date(5, v.to_string()),
            Some(StandardTag::ReleaseYear(y)) => tags.set_date(4, y.to_string()),
            Some(StandardTag::RecordingDate(v)) => tags.set_date(3, v.to_string()),
            Some(StandardTag::RecordingYear(y)) => tags.set_date(2, y.to_string()),
            Some(StandardTag::OriginalReleaseDate(v)) => tags.set_date(1, v.to_string()),
            Some(StandardTag::OriginalReleaseYear(y)) => tags.set_date(0, y.to_string()),
            _ => {}
        }
    }

    for visual in &container.visuals {
        if visual.data.is_empty() {
            continue;
        }
        let rank = match visual.usage {
            Some(StandardVisualKey::FrontCover) => 3,
            None | Some(StandardVisualKey::Other) => 2,
            Some(StandardVisualKey::FileIcon) | Some(StandardVisualKey::OtherIcon) => 0,
            Some(_) => 1,
        };
        if cover.as_ref().is_none_or(|c| rank >= c.rank) {
            *cover = Some(Cover {
                rank,
                data: visual.data.to_vec(),
            });
        }
    }
}

/// Decode embedded cover art and downscale it. Any failure means "no
/// thumbnail" — a broken picture frame must not sink the whole preview.
#[cfg(feature = "audio")]
fn decode_cover(data: &[u8], max_dim: u32) -> Option<image::RgbaImage> {
    use image::imageops::FilterType;
    use image::GenericImageView;

    let img = image::load_from_memory(data).ok()?;
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let img = if w > max_dim || h > max_dim {
        img.resize(max_dim, max_dim, FilterType::Triangle)
    } else {
        img
    };
    Some(img.to_rgba8())
}

// ---------------------------------------------------------------------------
// Formatters
// ---------------------------------------------------------------------------

/// `M:SS`, or `H:MM:SS` once the hour mark is crossed.
#[cfg(feature = "audio")]
fn format_duration(secs: f64) -> String {
    let total = if secs.is_finite() && secs > 0.0 {
        secs.round() as u64
    } else {
        0
    };
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// `44100` -> `44.1 kHz`, `48000` -> `48 kHz`.
#[cfg(feature = "audio")]
fn format_sample_rate(hz: u32) -> String {
    if hz.is_multiple_of(1000) {
        format!("{} kHz", hz / 1000)
    } else {
        // Trim to the significant fraction: 44100 -> 44.1, 22050 -> 22.05.
        let khz = f64::from(hz) / 1000.0;
        let mut s = format!("{khz:.3}");
        while s.ends_with('0') {
            s.pop();
        }
        s = s.trim_end_matches('.').to_string();
        format!("{s} kHz")
    }
}

#[cfg(feature = "audio")]
fn format_channels(count: usize) -> String {
    match count {
        0 => String::new(),
        1 => "Mono".to_string(),
        2 => "Stereo".to_string(),
        n => format!("{n} channels"),
    }
}

/// Overall bitrate = whole file over its playing time, so container overhead
/// and tags are included. That matches what other tools report.
#[cfg(feature = "audio")]
fn overall_bitrate(file_size: Option<u64>, duration_secs: Option<f64>) -> Option<String> {
    let (size, secs) = (file_size?, duration_secs?);
    if !secs.is_finite() || secs <= 0.0 {
        return None;
    }
    let kbps = (size as f64 * 8.0 / secs / 1000.0).round();
    if !kbps.is_finite() || kbps <= 0.0 {
        return None;
    }
    Some(format!("{kbps} kbps"))
}

/// Mirrors `human_size` in sekio-cli (core must not depend on a frontend).
#[cfg(feature = "audio")]
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Human name for a codec ID. The codec *registry* only knows codecs whose
/// decoders are compiled in, and sekio compiles none of them (metadata only),
/// so ask it first and fall back to the well-known ID table.
#[cfg(feature = "audio")]
fn codec_name(id: symphonia::core::codecs::audio::AudioCodecId) -> Option<&'static str> {
    use symphonia::core::codecs::audio::well_known::*;
    use symphonia::core::codecs::audio::CODEC_ID_NULL_AUDIO;

    if let Some(dec) = symphonia::default::get_codecs().get_audio_decoder(id) {
        return Some(dec.codec.info.long_name);
    }

    let name = match id {
        CODEC_ID_NULL_AUDIO => return None,

        CODEC_ID_PCM_S32LE
        | CODEC_ID_PCM_S32LE_PLANAR
        | CODEC_ID_PCM_S32BE
        | CODEC_ID_PCM_S32BE_PLANAR
        | CODEC_ID_PCM_S24LE
        | CODEC_ID_PCM_S24LE_PLANAR
        | CODEC_ID_PCM_S24BE
        | CODEC_ID_PCM_S24BE_PLANAR
        | CODEC_ID_PCM_S16LE
        | CODEC_ID_PCM_S16LE_PLANAR
        | CODEC_ID_PCM_S16BE
        | CODEC_ID_PCM_S16BE_PLANAR
        | CODEC_ID_PCM_S8
        | CODEC_ID_PCM_S8_PLANAR
        | CODEC_ID_PCM_U32LE
        | CODEC_ID_PCM_U32LE_PLANAR
        | CODEC_ID_PCM_U32BE
        | CODEC_ID_PCM_U32BE_PLANAR
        | CODEC_ID_PCM_U24LE
        | CODEC_ID_PCM_U24LE_PLANAR
        | CODEC_ID_PCM_U24BE
        | CODEC_ID_PCM_U24BE_PLANAR
        | CODEC_ID_PCM_U16LE
        | CODEC_ID_PCM_U16LE_PLANAR
        | CODEC_ID_PCM_U16BE
        | CODEC_ID_PCM_U16BE_PLANAR
        | CODEC_ID_PCM_U8
        | CODEC_ID_PCM_U8_PLANAR => "PCM",
        CODEC_ID_PCM_F32LE
        | CODEC_ID_PCM_F32LE_PLANAR
        | CODEC_ID_PCM_F32BE
        | CODEC_ID_PCM_F32BE_PLANAR
        | CODEC_ID_PCM_F64LE
        | CODEC_ID_PCM_F64LE_PLANAR
        | CODEC_ID_PCM_F64BE
        | CODEC_ID_PCM_F64BE_PLANAR => "PCM (float)",
        CODEC_ID_PCM_ALAW => "PCM A-law",
        CODEC_ID_PCM_MULAW => "PCM µ-law",

        CODEC_ID_ADPCM_G722 => "G.722 ADPCM",
        CODEC_ID_ADPCM_G726 | CODEC_ID_ADPCM_G726LE => "G.726 ADPCM",
        CODEC_ID_ADPCM_MS => "Microsoft ADPCM",
        CODEC_ID_ADPCM_IMA_WAV | CODEC_ID_ADPCM_IMA_QT => "IMA ADPCM",

        CODEC_ID_VORBIS => "Vorbis",
        CODEC_ID_OPUS => "Opus",
        CODEC_ID_SPEEX => "Speex",
        CODEC_ID_MUSEPACK => "Musepack",
        CODEC_ID_MP1 => "MP1",
        CODEC_ID_MP2 => "MP2",
        CODEC_ID_MP3 => "MP3",
        CODEC_ID_AAC => "AAC",
        CODEC_ID_AC3 => "AC-3",
        CODEC_ID_EAC3 => "E-AC-3",
        CODEC_ID_AC4 => "AC-4",
        CODEC_ID_DCA => "DTS",
        CODEC_ID_ATRAC1 => "ATRAC1",
        CODEC_ID_ATRAC3 => "ATRAC3",
        CODEC_ID_ATRAC3PLUS => "ATRAC3plus",
        CODEC_ID_ATRAC9 => "ATRAC9",
        CODEC_ID_WMA => "Windows Media Audio",
        CODEC_ID_RA10 => "RealAudio 1.0",
        CODEC_ID_RA20 => "RealAudio 2.0",
        CODEC_ID_SIPR => "RealAudio SIPR",
        CODEC_ID_COOK => "RealAudio Cook",
        CODEC_ID_SBC => "SBC",
        CODEC_ID_APTX => "aptX",
        CODEC_ID_APTX_HD => "aptX HD",
        CODEC_ID_LDAC => "LDAC",
        CODEC_ID_BINK_AUDIO => "Bink Audio",
        CODEC_ID_SMACKER_AUDIO => "Smacker Audio",

        CODEC_ID_FLAC => "FLAC",
        CODEC_ID_WAVPACK => "WavPack",
        CODEC_ID_MONKEYS_AUDIO => "Monkey's Audio",
        CODEC_ID_ALAC => "ALAC",
        CODEC_ID_TTA => "TTA",
        CODEC_ID_RALF => "RealAudio Lossless",
        CODEC_ID_TRUEHD => "Dolby TrueHD",

        _ => return None,
    };
    Some(name)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "audio"))]
mod tests {
    use super::*;
    use crate::PreviewContent;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn duration_formats_as_m_ss_then_h_mm_ss() {
        assert_eq!(format_duration(0.0), "0:00");
        assert_eq!(format_duration(9.4), "0:09");
        assert_eq!(format_duration(95.0), "1:35");
        assert_eq!(format_duration(599.0), "9:59");
        assert_eq!(format_duration(3600.0), "1:00:00");
        assert_eq!(format_duration(3661.0), "1:01:01");
        assert_eq!(format_duration(7325.0), "2:02:05");
        // Nonsense in, harmless out — never a panic.
        assert_eq!(format_duration(-5.0), "0:00");
        assert_eq!(format_duration(f64::NAN), "0:00");
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn sample_rates_are_human_readable() {
        assert_eq!(format_sample_rate(44100), "44.1 kHz");
        assert_eq!(format_sample_rate(48000), "48 kHz");
        assert_eq!(format_sample_rate(8000), "8 kHz");
        assert_eq!(format_sample_rate(22050), "22.05 kHz");
        assert_eq!(format_sample_rate(96000), "96 kHz");
    }

    #[test]
    fn channel_counts_read_naturally() {
        assert_eq!(format_channels(1), "Mono");
        assert_eq!(format_channels(2), "Stereo");
        assert_eq!(format_channels(6), "6 channels");
    }

    #[test]
    fn bitrate_needs_a_size_and_a_positive_duration() {
        assert_eq!(
            overall_bitrate(Some(176_444), Some(1.0)).as_deref(),
            Some("1412 kbps")
        );
        assert!(overall_bitrate(None, Some(1.0)).is_none());
        assert!(overall_bitrate(Some(1024), None).is_none());
        assert!(overall_bitrate(Some(1024), Some(0.0)).is_none());
    }

    // --- fixtures ----------------------------------------------------------

    struct TempFile(std::path::PathBuf);

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn temp_file(ext: &str, bytes: &[u8]) -> TempFile {
        static N: AtomicU32 = AtomicU32::new(0);
        let name = format!(
            "sekio-audio-test-{}-{}.{ext}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, bytes).expect("write fixture");
        TempFile(path)
    }

    /// A minimal canonical 44-byte-header RIFF/WAVE file with silent samples.
    fn wav_bytes(sample_rate: u32, channels: u16, bits: u16, frames: u32) -> Vec<u8> {
        let block_align = channels * (bits / 8);
        let data_len = frames * u32::from(block_align);
        let byte_rate = sample_rate * u32::from(block_align);

        let mut v = Vec::with_capacity(44 + data_len as usize);
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data_len).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // WAVE_FORMAT_PCM
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&sample_rate.to_le_bytes());
        v.extend_from_slice(&byte_rate.to_le_bytes());
        v.extend_from_slice(&block_align.to_le_bytes());
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        v.resize(44 + data_len as usize, 0);
        v
    }

    fn field<'a>(fields: &'a [crate::MetaField], key: &str) -> Option<&'a str> {
        fields
            .iter()
            .find(|f| f.key == key)
            .map(|f| f.value.as_str())
    }

    fn preview(file: &TempFile, mime: &str) -> Result<Preview, PreviewError> {
        render(
            &file.0,
            mime,
            Vec::new(),
            &PreviewOptions::default(),
            &CancelToken::new(),
        )
    }

    // --- behaviour ---------------------------------------------------------

    #[test]
    fn wav_reports_technical_fields() {
        // Exactly one second of 44.1 kHz 16-bit stereo silence.
        let file = temp_file("wav", &wav_bytes(44100, 2, 16, 44100));
        let preview = preview(&file, "audio/x-wav").expect("wav should preview");

        let PreviewContent::Metadata { fields, thumbnail } = preview.content else {
            panic!("audio must render as Metadata");
        };

        assert_eq!(field(&fields, "Sample Rate"), Some("44.1 kHz"));
        assert_eq!(field(&fields, "Channels"), Some("Stereo"));
        assert_eq!(field(&fields, "Duration"), Some("0:01"));
        assert_eq!(field(&fields, "Bit Depth"), Some("16-bit"));
        // With decoders registered, symphonia supplies a descriptive long name
        // ("PCM Signed 16-bit Little-Endian Interleaved"); the local table is
        // only the fallback. Assert the family, not the exact wording.
        assert!(
            field(&fields, "Codec").is_some_and(|c| c.starts_with("PCM")),
            "expected a PCM codec name, got {:?}",
            field(&fields, "Codec")
        );
        assert_eq!(field(&fields, "Type"), Some("audio/x-wav"));
        assert_eq!(field(&fields, "Size"), Some("172.3 KB"));
        assert!(field(&fields, "Bitrate").is_some());
        // No tags and no picture in a bare RIFF file: those fields are absent,
        // not blank.
        assert!(field(&fields, "Title").is_none());
        assert!(field(&fields, "Artist").is_none());
        assert!(thumbnail.is_none());
        assert!(!preview.truncated);
    }

    #[test]
    fn longer_wav_crosses_the_minute_mark() {
        // 8 kHz mono keeps the fixture small while still being 95 seconds long.
        let file = temp_file("wav", &wav_bytes(8000, 1, 8, 8000 * 95));
        let preview = preview(&file, "audio/x-wav").expect("wav should preview");
        let PreviewContent::Metadata { fields, .. } = preview.content else {
            panic!("audio must render as Metadata");
        };
        assert_eq!(field(&fields, "Duration"), Some("1:35"));
        assert_eq!(field(&fields, "Channels"), Some("Mono"));
        assert_eq!(field(&fields, "Sample Rate"), Some("8 kHz"));
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        let junk: Vec<u8> = (0u32..4096)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        let file = temp_file("mp3", &junk);
        assert!(matches!(
            preview(&file, "audio/mpeg"),
            Err(PreviewError::Format(_))
        ));
    }

    #[test]
    fn truncated_wav_is_an_error_not_a_panic() {
        let mut bytes = wav_bytes(44100, 2, 16, 1000);
        bytes.truncate(30); // cut inside the fmt chunk
        let file = temp_file("wav", &bytes);
        assert!(matches!(
            preview(&file, "audio/x-wav"),
            Err(PreviewError::Format(_))
        ));
    }

    #[test]
    fn empty_file_is_an_error_not_a_panic() {
        let file = temp_file("wav", b"");
        assert!(matches!(
            preview(&file, "audio/x-wav"),
            Err(PreviewError::Format(_))
        ));
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let path = std::env::temp_dir().join("sekio-audio-test-does-not-exist.wav");
        let err = render(
            &path,
            "audio/x-wav",
            Vec::new(),
            &PreviewOptions::default(),
            &CancelToken::new(),
        )
        .expect_err("missing file must fail");
        assert!(matches!(err, PreviewError::Io(_)));
    }

    #[test]
    fn a_cancelled_token_aborts_and_is_never_swallowed() {
        let file = temp_file("wav", &wav_bytes(44100, 2, 16, 44100));
        let cancel = CancelToken::new();
        cancel.cancel();
        let err = render(
            &file.0,
            "audio/x-wav",
            Vec::new(),
            &PreviewOptions::default(),
            &cancel,
        )
        .expect_err("a cancelled preview must not succeed");
        assert!(matches!(err, PreviewError::Cancelled));
    }

    #[test]
    fn undecodable_cover_art_yields_no_thumbnail() {
        assert!(decode_cover(b"", 512).is_none());
        assert!(decode_cover(b"not an image at all", 512).is_none());
    }

    #[test]
    fn cover_art_is_decoded_and_downscaled() {
        // Encode a 40x20 PNG in memory, then round-trip it through the same
        // path embedded art takes.
        let src = image::RgbaImage::from_pixel(40, 20, image::Rgba([10, 20, 30, 255]));
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(src)
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("encode png");
        let png = png.into_inner();

        let full = decode_cover(&png, 512).expect("decodes");
        assert_eq!((full.width(), full.height()), (40, 20));

        let small = decode_cover(&png, 10).expect("decodes");
        assert_eq!(small.width().max(small.height()), 10);
    }
}
