use std::{task::Poll, time::Instant};

use anyhow::{Context, Result};
use ffmpeg_next::{
    self as ffmpeg, Error, Packet, codec::Id, format::Pixel, frame::Video as VideoFrame,
};
use nokhwa::nokhwa_initialize;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};
use xcap::{Monitor, VideoRecorder};

use crate::video::H264Encoder;

// mod camera;

#[derive(Debug, derive_more::Display, Clone, Copy, Eq, PartialEq, Hash)]
pub enum CaptureSource {
    Screen,
    Camera,
}

pub enum Capturer {
    Screen(ScreenCapturer),
    Camera(CameraCapturer),
}

impl Capturer {
    pub fn new(source: CaptureSource) -> Result<Self> {
        Ok(match source {
            CaptureSource::Screen => Self::Screen(ScreenCapturer::new()?),
            CaptureSource::Camera => Self::Camera(CameraCapturer::new()?),
        })
    }

    pub fn width(&mut self) -> u32 {
        match self {
            Capturer::Screen(inner) => inner.width,
            Capturer::Camera(inner) => inner.width,
        }
    }

    pub fn height(&mut self) -> u32 {
        match self {
            Capturer::Screen(inner) => inner.height,
            Capturer::Camera(inner) => inner.height,
        }
    }

    /// Capture image.
    ///
    /// Should be called in a loop, otherwise at least the screen capturer on linux/pipewire
    /// amasses memory in a channel of frames.
    pub fn capture(&mut self) -> Result<VideoFrame> {
        let frame = match self {
            Self::Screen(capturer) => capturer.capture()?,
            Self::Camera(capturer) => capturer.capture()?,
        };
        Ok(frame)
    }
}

pub struct ScreenCapturer {
    pub(crate) _monitor: Monitor,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) video_recorder: VideoRecorder,
    pub(crate) rx: std::sync::mpsc::Receiver<xcap::Frame>,
}

impl Drop for ScreenCapturer {
    fn drop(&mut self) {
        self.video_recorder.stop().ok();
    }
}

impl ScreenCapturer {
    pub fn new() -> Result<Self> {
        info!("Initializing screen capturer with xcap");

        let monitors = Monitor::all().context("Failed to get monitors")?;
        if monitors.is_empty() {
            return Err(anyhow::anyhow!("No monitors available"));
        }
        debug!("Monitors: {monitors:?}");

        let monitor = monitors.into_iter().next().unwrap();
        let width = monitor.width()?;
        let height = monitor.height()?;
        let name = monitor
            .name()
            .unwrap_or_else(|_| "Unknown Monitor".to_string());

        info!("Using primary monitor: {} ({}x{})", name, width, height);

        let (video_recorder, rx) = monitor.video_recorder()?;
        video_recorder.start()?;

        Ok(Self {
            _monitor: monitor,
            video_recorder,
            rx,
            width,
            height,
        })
    }

    pub fn capture(&mut self) -> Result<VideoFrame> {
        let mut raw_frame = None;
        // We are only interested in the latest frame.
        // Drain the channel to not build up memory.
        while let Ok(next) = self.rx.try_recv() {
            raw_frame = Some(next)
        }
        let raw_frame = match raw_frame {
            Some(frame) => frame,
            None => self
                .rx
                .recv()
                .context("video recorder did not produce new frame")?,
        };
        let mut frame = VideoFrame::new(Pixel::RGBA, raw_frame.width, raw_frame.height);
        frame.data_mut(0).copy_from_slice(&raw_frame.raw);
        Ok(frame)
    }
}

pub struct CameraCapturer {
    pub(crate) camera: nokhwa::Camera,
    pub(crate) width: u32,
    pub(crate) height: u32,
    decoder: MjpgDecoder,
}

impl CameraCapturer {
    pub fn new() -> Result<Self> {
        info!("Initializing camera capturer");
        nokhwa_initialize(|granted| {
            debug!("User said {}", granted);
        });

        let cameras = nokhwa::query(nokhwa::utils::ApiBackend::Auto)?;
        if cameras.is_empty() {
            return Err(anyhow::anyhow!("No cameras available"));
        }
        debug!("cameras: {cameras:?}");

        let first_camera = cameras.last().unwrap();
        info!("Using camera: {}", first_camera.human_name());

        let mut camera = nokhwa::Camera::new(
            first_camera.index().clone(),
            nokhwa::utils::RequestedFormat::new::<nokhwa::pixel_format::RgbFormat>(
                nokhwa::utils::RequestedFormatType::AbsoluteHighestFrameRate,
            ),
        )?;

        camera.open_stream()?;

        // Get actual resolution
        let resolution = camera.resolution();
        info!(
            "Camera resolution: {}x{}",
            resolution.width(),
            resolution.height()
        );

        Ok(Self {
            camera,
            width: resolution.width(),
            height: resolution.height(),
            decoder: MjpgDecoder::new()?,
        })
    }

    pub fn capture(&mut self) -> Result<VideoFrame> {
        // let start = Instant::now();
        let frame = self
            .camera
            .frame()
            .context("Failed to capture camera frame")?;
        // let t_capture = start.elapsed();
        // TODO: Don't decode here but in ffmpeg?
        // let image = frame
        //     .decode_image::<nokhwa::pixel_format::RgbAFormat>()
        //     .context("Failed to decode camera frame")?;
        // let mut frame = VideoFrame::new(Pixel::RGBA, image.width(), image.height());
        // frame.data_mut(0).copy_from_slice(&image.as_raw());
        let frame = self.decoder.decode_frame(&frame.buffer())?;
        // let t_decode = start.elapsed() - t_capture;

        // println!("camera frame {t_capture:?} {t_decode:?}",);
        Ok(frame)
    }
}

pub struct CaptureEncoder {
    pub source: CaptureSource,
    pub capturer: Capturer,
    pub encoder: H264Encoder,
    shutdown: CancellationToken,
    fps: u32,
}

impl CaptureEncoder {
    pub fn new(source: CaptureSource, shutdown: CancellationToken) -> Result<Self> {
        let fps = 30;
        let mut capturer = Capturer::new(source).context("failed to open capture source")?;
        let encoder = H264Encoder::new(capturer.width(), capturer.height(), fps)
            .context("failed to create video encoder")?;
        Ok(Self {
            source,
            capturer,
            encoder,
            shutdown,
            fps,
        })
    }

    pub fn run(&mut self, mut on_frame: impl FnMut(hang::Frame)) -> Result<()> {
        let interval = std::time::Duration::from_secs(1) / self.fps; // ~30 FPS
        let mut i = 0;
        'run: loop {
            let start = Instant::now();
            // Check shutdown before each frame capture
            if self.shutdown.is_cancelled() {
                debug!("Video capture shutdown requested");
                break;
            }

            let frame = self.capturer.capture()?;
            let t_capture = start.elapsed();
            self.encoder.encode_frame(frame)?;
            let t_encode = start.elapsed() - t_capture;
            while let Poll::Ready(frame) = self.encoder.receive_packet()? {
                match frame {
                    Some(frame) => on_frame(frame),
                    None => break 'run,
                }
            }
            let t_fwd = start.elapsed() - t_encode - t_capture;
            let t_total = start.elapsed();
            let t_sleep = interval.saturating_sub(start.elapsed());
            if i % 20 == 0 {
                tracing::trace!(
                    ?t_capture,
                    ?t_encode,
                    ?t_fwd,
                    ?t_total,
                    ?t_sleep,
                    "source {} frame {i}",
                    self.source
                );
            }
            std::thread::sleep(interval.saturating_sub(start.elapsed()));
            i += 1;
        }

        // Flush remaining frames on shutdown
        self.encoder.flush()?;
        while let Poll::Ready(Some(frame)) = self.encoder.receive_packet()? {
            on_frame(frame);
        }
        Ok(())
    }
}

pub struct MjpgDecoder {
    dec: ffmpeg::decoder::Video, // <- video decoder, not codec::Context
}

impl MjpgDecoder {
    /// Initialize FFmpeg and create a Video decoder for MJPEG.
    pub fn new() -> Result<Self, Error> {
        ffmpeg::init()?;

        // Find the MJPEG decoder and create a context bound to it.
        let mjpeg = ffmpeg::decoder::find(Id::MJPEG).ok_or(Error::DecoderNotFound)?;

        // Create a codec::Context that's pre-bound to this decoder codec,
        // then get a video decoder out of it.
        let ctx = ffmpeg::codec::context::Context::new_with_codec(mjpeg);
        let dec = ctx.decoder().video()?; // has send_packet/receive_frame

        Ok(Self { dec })
    }

    /// Decode one complete MJPEG/JPEG frame from `mjpg_frame`.
    pub fn decode_frame(&mut self, mjpg_frame: &[u8]) -> Result<VideoFrame, Error> {
        // Make a packet that borrows/copies the data.
        let packet = Packet::borrow(mjpg_frame);
        // Feed & drain once — MJPEG is intra-only (one picture per packet).
        self.dec.send_packet(&packet)?;
        let mut frame = VideoFrame::empty();
        self.dec.receive_frame(&mut frame)?;

        // MJPEG may output deprecated YUVJ* formats. Replace them with
        // the non-deprecated equivalents and mark full range to keep semantics.
        // This avoids ffmpeg warning: "deprecated pixel format used, make sure you did set range correctly".
        use ffmpeg_next::util::color::Range;
        match frame.format() {
            Pixel::YUVJ420P => {
                frame.set_color_range(Range::JPEG);
                frame.set_format(Pixel::YUV420P);
            }
            Pixel::YUVJ422P => {
                frame.set_color_range(Range::JPEG);
                frame.set_format(Pixel::YUV422P);
            }
            Pixel::YUVJ444P => {
                frame.set_color_range(Range::JPEG);
                frame.set_format(Pixel::YUV444P);
            }
            _ => {}
        }
        Ok(frame)
    }
}
