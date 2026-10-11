use super::*;

/// Which downloader handles a job. The textual form ([`Engine::as_str`]) is the
/// stable identifier of the command line (`--engine`), nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Engine {
    YtDlp,
    Aria2,
}

impl Engine {
    pub const ALL: [Engine; 2] = [Engine::YtDlp, Engine::Aria2];

    pub fn as_str(self) -> &'static str {
        match self {
            Engine::YtDlp => "yt-dlp",
            Engine::Aria2 => "aria2",
        }
    }

    pub fn parse(text: &str) -> Option<Engine> {
        Engine::ALL.into_iter().find(|e| e.as_str() == text)
    }

    /// The label shown when the user picks an engine.
    pub fn label(self) -> &'static str {
        match self {
            Engine::YtDlp => tr!(
                "yt-dlp — video, audio and streams from thousands of sites",
                "yt-dlp — видео, аудио и стримы с тысяч сайтов"
            ),
            Engine::Aria2 => tr!(
                "aria2c — direct links, torrents, magnets",
                "aria2c — прямые ссылки, torrent, magnet"
            ),
        }
    }
}

impl std::fmt::Display for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What yt-dlp is asked to fetch. Other engines ignore it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Format {
    #[default]
    Best,
    P2160,
    P1440,
    P1080,
    P720,
    P480,
    /// Audio converted to mp3.
    Audio,
    /// The original audio stream, not converted.
    AudioSource,
    /// Subtitles only (`.srt`), no media.
    Subs,
}

impl Format {
    pub const ALL: [Format; 9] = [
        Format::Best,
        Format::P2160,
        Format::P1440,
        Format::P1080,
        Format::P720,
        Format::P480,
        Format::Audio,
        Format::AudioSource,
        Format::Subs,
    ];

    /// The stable identifier of the command line (`--format`).
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Best => "best",
            Format::P2160 => "2160p",
            Format::P1440 => "1440p",
            Format::P1080 => "1080p",
            Format::P720 => "720p",
            Format::P480 => "480p",
            Format::Audio => "audio",
            Format::AudioSource => "audio-src",
            Format::Subs => "subs",
        }
    }

    pub fn parse(text: &str) -> Option<Format> {
        Format::ALL.into_iter().find(|f| f.as_str() == text)
    }

    /// The label shown when the user picks a format.
    pub fn label(self) -> &'static str {
        match self {
            Format::Best => tr!("Best quality", "Лучшее качество"),
            Format::P2160 => tr!("Video up to 2160p (4K)", "Видео до 2160p (4K)"),
            Format::P1440 => tr!("Video up to 1440p", "Видео до 1440p"),
            Format::P1080 => tr!("Video up to 1080p", "Видео до 1080p"),
            Format::P720 => tr!("Video up to 720p", "Видео до 720p"),
            Format::P480 => tr!("Video up to 480p", "Видео до 480p"),
            Format::Audio => tr!("Audio — mp3 (conversion)", "Аудио — mp3 (перекодирование)"),
            Format::AudioSource => tr!(
                "Audio — original (no conversion)",
                "Аудио — исходное (без перекодирования)"
            ),
            Format::Subs => tr!(
                "Subtitles — .srt only (no video)",
                "Субтитры — только .srt (без видео)"
            ),
        }
    }

    /// Whether the result is audio (artwork and tags are embedded for these).
    pub fn is_audio(self) -> bool {
        matches!(self, Format::Audio | Format::AudioSource)
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

pub fn formats() -> [(Format, &'static str); 9] {
    Format::ALL.map(|format| (format, format.label()))
}

/// Source-aware UI choices. Explicit CLI format flags are not restricted:
/// these are presets, not a substitute for inspecting a particular item.
pub fn formats_for_url(url: &str) -> Vec<(Format, &'static str)> {
    use crate::links::{media_preset, MediaPreset};
    let preset = media_preset(url);
    formats()
        .into_iter()
        .filter(|(format, _)| {
            preset == MediaPreset::General
                || matches!(format, Format::Best | Format::Audio | Format::AudioSource)
        })
        .map(|(format, label)| (format, preset_label(preset, format).unwrap_or(label)))
        .collect()
}

pub(super) fn preset_label(
    preset: crate::links::MediaPreset,
    format: Format,
) -> Option<&'static str> {
    use crate::links::MediaPreset;
    match (preset, format) {
        (MediaPreset::Instagram, Format::Best) => {
            Some(tr!("Video — best available", "Видео — лучшее доступное"))
        }
        (MediaPreset::Instagram, Format::Audio) => Some(tr!(
            "Audio only — mp3 (conversion)",
            "Только звук — mp3 (перекодирование)"
        )),
        (MediaPreset::Instagram, Format::AudioSource) => Some(tr!(
            "Audio only — as-is (no conversion)",
            "Только звук — как есть (без перекодирования)"
        )),
        (MediaPreset::Audio, Format::Best) => Some(tr!(
            "Audio — as-is (no conversion)",
            "Аудио — как есть (без перекодирования)"
        )),
        (MediaPreset::Audio, Format::AudioSource) => Some(tr!(
            "Audio — as-is with tags and cover",
            "Аудио — как есть + теги и обложка"
        )),
        (MediaPreset::Audio, Format::Audio) => Some(tr!(
            "Audio — mp3 (conversion)",
            "Аудио — mp3 (перекодирование)"
        )),
        _ => None,
    }
}

pub fn ui_format_for_url(url: &str, format: Format) -> Format {
    if formats_for_url(url).iter().any(|(key, _)| *key == format) {
        format
    } else {
        Format::Best
    }
}
