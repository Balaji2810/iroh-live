use std::time::Duration;

use anyhow::Result;
use image::RgbaImage;
use strum::{Display, EnumString, VariantNames};

#[derive(Copy, Clone, Debug)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channel_count: u32,
}

pub trait AudioSource: Send + 'static {
    fn cloned_boxed(&self) -> Box<dyn AudioSource>;
    fn format(&self) -> AudioFormat;
    fn pop_samples(&mut self, buf: &mut [f32]) -> Result<Option<usize>>;
}

impl AudioSource for Box<dyn AudioSource> {
    fn cloned_boxed(&self) -> Box<dyn AudioSource> {
        (&**self).cloned_boxed()
    }

    fn format(&self) -> AudioFormat {
        (&**self).format()
    }

    fn pop_samples(&mut self, buf: &mut [f32]) -> Result<Option<usize>> {
        (&mut **self).pop_samples(buf)
    }
}

pub trait AudioSink: Send + 'static {
    fn format(&self) -> Result<AudioFormat>;
    fn push_samples(&mut self, buf: &[f32]) -> Result<()>;
}

pub trait AudioEncoder: AudioEncoderInner {
    fn with_preset(preset: AudioPreset) -> Result<Self>
    where
        Self: Sized;
}
pub trait AudioEncoderInner: Send + 'static {
    fn config(&self) -> hang::catalog::AudioConfig;
    fn push_samples(&mut self, samples: &[f32]) -> Result<()>;
    fn pop_packet(&mut self) -> Result<Option<hang::Frame>>;
}

impl AudioEncoderInner for Box<dyn AudioEncoder> {
    fn config(&self) -> hang::catalog::AudioConfig {
        (&**self).config()
    }

    fn push_samples(&mut self, samples: &[f32]) -> Result<()> {
        (&mut **self).push_samples(samples)
    }

    fn pop_packet(&mut self) -> Result<Option<hang::Frame>> {
        (&mut **self).pop_packet()
    }
}

pub trait AudioDecoder: Send + 'static {
    fn new(config: &hang::catalog::AudioConfig, target_format: AudioFormat) -> Result<Self>
    where
        Self: Sized;
    fn push_packet(&mut self, packet: hang::Frame) -> Result<()>;
    fn pop_samples(&mut self) -> Result<Option<&[f32]>>;
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum PixelFormat {
    Rgba,
    Bgra,
}

impl Default for PixelFormat {
    fn default() -> Self {
        PixelFormat::Rgba
    }
}

#[derive(Clone, Debug)]
pub struct VideoFormat {
    pub pixel_format: PixelFormat,
    pub dimensions: [u32; 2],
}

#[derive(Clone, Debug)]
pub struct VideoFrame {
    pub format: VideoFormat,
    pub raw: Vec<u8>,
}

pub trait VideoSource: Send + 'static {
    fn format(&self) -> VideoFormat;
    fn pop_frame(&mut self) -> Result<Option<VideoFrame>>;
}

pub trait VideoEncoder: VideoEncoderInner {
    fn with_preset(preset: VideoPreset) -> Result<Self>
    where
        Self: Sized;

    fn new(width: u32, height: u32, framerate: u32) -> Result<Self>
    where
        Self: Sized;
}

pub trait VideoEncoderInner: Send + 'static {
    fn config(&self) -> hang::catalog::VideoConfig;
    fn push_frame(&mut self, frame: VideoFrame) -> Result<()>;
    fn pop_packet(&mut self) -> Result<Option<hang::Frame>>;
}

impl VideoEncoderInner for Box<dyn VideoEncoder> {
    fn config(&self) -> hang::catalog::VideoConfig {
        (&**self).config()
    }

    fn push_frame(&mut self, frame: VideoFrame) -> Result<()> {
        (&mut **self).push_frame(frame)
    }

    fn pop_packet(&mut self) -> Result<Option<hang::Frame>> {
        (&mut **self).pop_packet()
    }
}

pub trait VideoDecoder: Send + 'static {
    fn new(config: &hang::catalog::VideoConfig, playback_config: &PlaybackConfig) -> Result<Self>
    where
        Self: Sized;
    fn pop_frame(&mut self) -> Result<Option<DecodedFrame>>;
    fn push_packet(&mut self, packet: hang::Frame) -> Result<()>;
    fn set_viewport(&mut self, w: u32, h: u32);
}

pub struct DecodedFrame {
    pub frame: image::Frame,
    pub timestamp: Duration,
}

impl DecodedFrame {
    pub fn img(&self) -> &RgbaImage {
        self.frame.buffer()
    }
}

#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub enum TrackKind {
    Audio,
    Video,
}

impl TrackKind {
    pub fn from_name(name: &str) -> Option<Self> {
        if name.starts_with("audio-") {
            Some(Self::Audio)
        } else if name.starts_with("video-") {
            Some(Self::Video)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, Display, EnumString, VariantNames)]
#[strum(serialize_all = "lowercase")]
pub enum AudioCodec {
    Opus,
}

#[derive(Debug, Clone, Copy, Display, EnumString, VariantNames)]
#[strum(serialize_all = "lowercase")]
pub enum VideoCodec {
    H264,
    Av1,
}

/// Video quality preset - dimensions are calculated dynamically based on source aspect ratio
#[derive(Debug, Clone, Copy, Display, EnumString, VariantNames, Eq, PartialEq, Ord, PartialOrd)]
pub enum VideoPreset {
    #[strum(serialize = "poor")]
    Poor,
    #[strum(serialize = "medium")]
    Medium,
    #[strum(serialize = "good")]
    Good,
    #[strum(serialize = "best")]
    Best,
}

impl VideoPreset {
    pub fn all() -> [VideoPreset; 4] {
        [Self::Poor, Self::Medium, Self::Good, Self::Best]
    }

    /// Get base height for this quality level (width computed from aspect ratio)
    pub fn base_height(&self) -> u32 {
        match self {
            Self::Poor => 360,
            Self::Medium => 720,
            Self::Good => 1080,
            Self::Best => 1440,
        }
    }

    /// Calculate dimensions for a given source aspect ratio, preserving native ratio
    pub fn dimensions_for_aspect(&self, source_width: u32, source_height: u32) -> (u32, u32) {
        let height = self.base_height();
        // Preserve source aspect ratio
        let aspect = source_width as f32 / source_height as f32;
        let width = ((height as f32 * aspect) as u32 / 2) * 2; // Ensure even number for encoder
        (width, height)
    }

    /// Default dimensions (16:9 aspect ratio) - used when source aspect is unknown
    pub fn dimensions(&self) -> (u32, u32) {
        self.dimensions_for_aspect(16, 9)
    }

    pub fn width(&self) -> u32 {
        self.dimensions().0
    }

    pub fn height(&self) -> u32 {
        self.dimensions().1
    }

    pub fn fps(&self) -> u32 {
        30
    }

    pub fn with_fps(self, fps: u32) -> VideoPresetWithFps {
        VideoPresetWithFps::new(self, fps)
    }

    /// Get human-readable display name for UI
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Poor => "Poor",
            Self::Medium => "Medium",
            Self::Good => "Good",
            Self::Best => "Best",
        }
    }
}

/// VideoPreset with configurable frame rate and source dimensions for aspect ratio preservation
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct VideoPresetWithFps {
    pub preset: VideoPreset,
    pub fps: u32,
    pub source_width: u32,
    pub source_height: u32,
}

impl VideoPresetWithFps {
    pub fn new(preset: VideoPreset, fps: u32) -> Self {
        Self { preset, fps, source_width: 16, source_height: 9 }
    }

    /// Create preset with source dimensions for aspect ratio preservation
    pub fn with_source_dimensions(preset: VideoPreset, fps: u32, source_width: u32, source_height: u32) -> Self {
        Self { preset, fps, source_width, source_height }
    }

    pub fn dimensions(&self) -> (u32, u32) {
        self.preset.dimensions_for_aspect(self.source_width, self.source_height)
    }

    pub fn width(&self) -> u32 {
        self.dimensions().0
    }

    pub fn height(&self) -> u32 {
        self.dimensions().1
    }

    pub fn fps(&self) -> u32 {
        self.fps
    }
}

impl std::fmt::Display for VideoPresetWithFps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}fps", self.preset, self.fps)
    }
}

#[derive(Debug, Clone, Copy, Display, EnumString, VariantNames, Eq, PartialEq)]
#[strum(serialize_all = "lowercase")]
pub enum AudioPreset {
    Hq,
    Lq,
}

#[derive(Debug, Clone, Copy, Display, EnumString, VariantNames, Eq, PartialEq)]
#[strum(serialize_all = "lowercase")]
pub enum Quality {
    Highest,
    High,
    Mid,
    Low,
}

#[derive(Clone, Default)]
pub struct PlaybackConfig {
    pub pixel_format: PixelFormat,
}
