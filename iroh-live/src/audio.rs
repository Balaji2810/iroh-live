use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rubato::{Resampler, FftFixedOut};
use firewheel::{
    CpalConfig, CpalOutputConfig, FirewheelConfig, FirewheelContext,
    channel_config::{ChannelCount, NonZeroChannelCount},
    graph::PortIdx,
    nodes::stream::{
        ResamplingChannelConfig,
        reader::{StreamReaderConfig, StreamReaderNode, StreamReaderState},
        writer::{StreamWriterConfig, StreamWriterNode, StreamWriterState},
    },
};
use hang::catalog::AudioConfig;
use tokio::sync::{mpsc, mpsc::error::TryRecvError, oneshot};
use tracing::{error, info, trace, warn};

use crate::av::{AudioFormat, AudioSink, AudioSource};

pub type OutputStreamHandle = Arc<Mutex<StreamWriterState>>;
pub type InputStreamHandle = Arc<Mutex<StreamReaderState>>;

#[derive(Clone)]
pub struct OutputControl {
    handle: OutputStreamHandle,
    paused: Arc<AtomicBool>,
}

impl AudioSink for OutputControl {
    fn format(&self) -> Result<AudioFormat> {
        let info = match self.handle.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::warn!("audio output stream lock poisoned, recovering");
                poisoned.into_inner()
            }
        };
        let sample_rate = info
            .sample_rate()
            .context("output stream misses sample rate")?
            .get();
        let channel_count = info.num_channels().get().get();
        Ok(AudioFormat {
            sample_rate,
            channel_count,
        })
    }

    fn push_samples(&mut self, samples: &[f32]) -> Result<()> {
        let mut handle = match self.handle.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::warn!("audio output stream lock poisoned, recovering");
                poisoned.into_inner()
            }
        };

        // If this happens excessively in Release mode, you may want to consider
        // increasing [`StreamWriterConfig::channel_config.latency_seconds`].
        if handle.underflow_occurred() {
            warn!("Underflow occured in stream writer node!");
        }

        // If this happens excessively in Release mode, you may want to consider
        // increasing [`StreamWriterConfig::channel_config.capacity_seconds`]. For
        // example, if you are streaming data from a network, you may want to
        // increase the capacity to several seconds.
        if handle.overflow_occurred() {
            warn!("Overflow occured in stream writer node!");
        }

        // Wait until the node's processor is ready to receive data.
        if handle.is_ready() {
            // let expected_bytes =
            //     frame.samples() * frame.channels() as usize * core::mem::size_of::<f32>();
            // let cpal_sample_data: &[f32] = bytemuck::cast_slice(&frame.data(0)[..expected_bytes]);
            handle.push_interleaved(samples);
            trace!("pushed samples {}", samples.len());
        } else {
            warn!("output handle is inactive")
        }
        Ok(())
    }
}

impl OutputControl {
    pub fn new(handle: OutputStreamHandle) -> Self {
        Self {
            handle,
            paused: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    #[allow(unused)]
    pub fn is_active(&self) -> bool {
        self.handle.lock().map(|h| h.is_active()).unwrap_or_else(|e| e.into_inner().is_active())
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
        let mut handle = self.handle.lock().unwrap_or_else(|e| e.into_inner());
        handle.pause_stream();
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::Relaxed);
        let mut handle = self.handle.lock().unwrap_or_else(|e| e.into_inner());
        handle.resume();
    }
}

/// A simple AudioSource that reads from the default microphone via Firewheel.
#[derive(Clone)]
pub struct MicrophoneSource {
    handle: InputStreamHandle,
    format: AudioFormat,
}

impl MicrophoneSource {
    // Kept for the Firewheel stream-reader path (currently not used by default).
    #[allow(dead_code)]
    pub(crate) fn new(handle: InputStreamHandle, sample_rate: u32, channel_count: u32) -> Self {
        Self {
            handle,
            format: AudioFormat {
                sample_rate,
                channel_count,
            },
        }
    }
}

/// Microphone input via CPAL, resampled to 48kHz stereo for the encoder pipeline.
///
/// This is intentionally separate from Firewheel's duplex stream so input/output devices
/// can have different hardware sample rates.
pub struct CpalMicrophoneSource {
    /// Always 48kHz stereo.
    format: AudioFormat,
    /// Interleaved f32 samples at native rate/channels from the CPAL callback.
    queue: Arc<Mutex<VecDeque<f32>>>,
    /// Native device rate.
    src_rate: u32,
    /// Native device channels.
    src_channels: usize,
    /// Resampler position in source frames.
    src_pos_frames: f32,
    /// Stops the capture thread (and drops the CPAL stream on that thread).
    stop: Arc<AtomicBool>,
    /// Keep the capture thread alive.
    capture_thread: Option<std::thread::JoinHandle<()>>,
}

impl CpalMicrophoneSource {
    pub fn new_48k_stereo() -> anyhow::Result<Self> {
        let queue = Arc::new(Mutex::new(VecDeque::<f32>::new()));
        let stop = Arc::new(AtomicBool::new(false));

        // We cannot store `cpal::Stream` inside this struct because `Stream` is not `Send`
        // on all platforms (notably Windows). Instead we run CPAL capture on a dedicated
        // thread and share samples through `queue`.
        let (init_tx, init_rx) = std::sync::mpsc::channel::<anyhow::Result<(u32, usize)>>();
        let queue_thread = queue.clone();
        let stop_thread = stop.clone();

        let capture_thread = std::thread::spawn(move || {
            let init: anyhow::Result<(u32, usize)> = (|| {
                let host = cpal::default_host();
                let device = host
                    .default_input_device()
                    .context("no default input device")?;
                let default_cfg = device
                    .default_input_config()
                    .context("no default input config")?;

                let src_rate = default_cfg.sample_rate().0;
                let src_channels = default_cfg.channels() as usize;
                if src_channels == 0 {
                    anyhow::bail!("input device reports 0 channels");
                }

                // Low-latency: bound buffer to ~500ms to avoid unbounded latency growth.
                let max_samples = (src_rate as usize) * src_channels / 2;

                let err_fn = |err| tracing::warn!("cpal input stream error: {err}");
                let cfg: cpal::StreamConfig = default_cfg.clone().into();

                let queue_for_callbacks = queue_thread.clone();
                let stream = match default_cfg.sample_format() {
                    cpal::SampleFormat::F32 => {
                        let queue_cb = queue_for_callbacks.clone();
                        device.build_input_stream(
                            &cfg,
                            move |data: &[f32], _| {
                                let mut q = queue_cb.lock().unwrap_or_else(|e| e.into_inner());
                                q.extend(data.iter().copied());
                                while q.len() > max_samples {
                                    q.pop_front();
                                }
                            },
                            err_fn,
                            None,
                        )?
                    }
                    cpal::SampleFormat::I16 => {
                        let queue_cb = queue_for_callbacks.clone();
                        device.build_input_stream(
                            &cfg,
                            move |data: &[i16], _| {
                                let mut q = queue_cb.lock().unwrap_or_else(|e| e.into_inner());
                                q.extend(data.iter().map(|&x| x as f32 / i16::MAX as f32));
                                while q.len() > max_samples {
                                    q.pop_front();
                                }
                            },
                            err_fn,
                            None,
                        )?
                    }
                    cpal::SampleFormat::U16 => {
                        let queue_cb = queue_for_callbacks.clone();
                        device.build_input_stream(
                            &cfg,
                            move |data: &[u16], _| {
                                let mut q = queue_cb.lock().unwrap_or_else(|e| e.into_inner());
                                q.extend(
                                    data.iter().map(|&x| (x as f32 / u16::MAX as f32) * 2.0 - 1.0),
                                );
                                while q.len() > max_samples {
                                    q.pop_front();
                                }
                            },
                            err_fn,
                            None,
                        )?
                    }
                    other => anyhow::bail!("unsupported CPAL sample format: {other:?}"),
                };

                stream.play()?;

                // Keep the stream alive on this thread by returning it to the outer scope.
                Ok((src_rate, src_channels))
            })();

            let init_ok = init.is_ok();
            let _ = init_tx.send(init);
            if !init_ok {
                return;
            }

            // Keep thread alive until stop is requested.
            // IMPORTANT: CPAL stream must stay alive; (re)create it here and keep it in scope.
            // We re-open the device/stream because the previous `stream` lived only inside the
            // init closure scope.
            let _stream = (|| -> anyhow::Result<cpal::Stream> {
                let host = cpal::default_host();
                let device = host
                    .default_input_device()
                    .context("no default input device")?;
                let default_cfg = device
                    .default_input_config()
                    .context("no default input config")?;

                let src_rate = default_cfg.sample_rate().0;
                let src_channels = default_cfg.channels() as usize;
                if src_channels == 0 {
                    anyhow::bail!("input device reports 0 channels");
                }
                let max_samples = (src_rate as usize) * src_channels / 2;

                let err_fn = |err| tracing::warn!("cpal input stream error: {err}");
                let cfg: cpal::StreamConfig = default_cfg.clone().into();
                let queue_for_callbacks = queue_thread.clone();

                let stream = match default_cfg.sample_format() {
                    cpal::SampleFormat::F32 => {
                        let queue_cb = queue_for_callbacks.clone();
                        device.build_input_stream(
                            &cfg,
                            move |data: &[f32], _| {
                                let mut q =
                                    queue_cb.lock().unwrap_or_else(|e| e.into_inner());
                                q.extend(data.iter().copied());
                                while q.len() > max_samples {
                                    q.pop_front();
                                }
                            },
                            err_fn,
                            None,
                        )?
                    }
                    cpal::SampleFormat::I16 => {
                        let queue_cb = queue_for_callbacks.clone();
                        device.build_input_stream(
                            &cfg,
                            move |data: &[i16], _| {
                                let mut q =
                                    queue_cb.lock().unwrap_or_else(|e| e.into_inner());
                                q.extend(data.iter().map(|&x| x as f32 / i16::MAX as f32));
                                while q.len() > max_samples {
                                    q.pop_front();
                                }
                            },
                            err_fn,
                            None,
                        )?
                    }
                    cpal::SampleFormat::U16 => {
                        let queue_cb = queue_for_callbacks.clone();
                        device.build_input_stream(
                            &cfg,
                            move |data: &[u16], _| {
                                let mut q =
                                    queue_cb.lock().unwrap_or_else(|e| e.into_inner());
                                q.extend(data.iter().map(|&x| {
                                    (x as f32 / u16::MAX as f32) * 2.0 - 1.0
                                }));
                                while q.len() > max_samples {
                                    q.pop_front();
                                }
                            },
                            err_fn,
                            None,
                        )?
                    }
                    other => anyhow::bail!("unsupported CPAL sample format: {other:?}"),
                };
                stream.play()?;
                Ok(stream)
            })()
            .inspect_err(|err| tracing::error!("cpal mic capture failed: {err:#}"))
            .ok();

            while !stop_thread.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
            // stream dropped here when thread exits (if init succeeded)
        });

        let (src_rate, src_channels) = init_rx
            .recv()
            .context("cpal mic init channel closed")??;

        Ok(Self {
            format: AudioFormat {
                sample_rate: 48_000,
                channel_count: 2,
            },
            src_rate,
            src_channels,
            queue,
            src_pos_frames: 0.0,
            stop,
            capture_thread: Some(capture_thread),
        })
    }

    #[inline]
    fn read_src(q: &VecDeque<f32>, frame: usize, ch: usize, src_channels: usize) -> f32 {
        q[frame * src_channels + ch]
    }
}

impl AudioSource for CpalMicrophoneSource {
    fn cloned_boxed(&self) -> Box<dyn AudioSource> {
        // Create a fresh CPAL stream for each clone; simple and avoids sharing non-Sync handles.
        Box::new(Self::new_48k_stereo().expect("failed to create CPAL microphone source"))
    }

    fn format(&self) -> AudioFormat {
        self.format
    }

    fn pop_samples(&mut self, buf: &mut [f32]) -> anyhow::Result<Option<usize>> {
        // Always output stereo interleaved.
        let out_frames = buf.len() / 2;
        buf.fill(0.0);

        if out_frames == 0 {
            return Ok(Some(0));
        }

        let ratio = self.src_rate as f32 / 48_000.0; // source frames per output frame
        let src_ch = self.src_channels;

        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let available_src_frames = q.len() / src_ch;
        if available_src_frames < 2 {
            // Keep pacing; output silence.
            return Ok(Some(buf.len()));
        }

        let mut produced_frames = 0usize;
        for i in 0..out_frames {
            let idx0 = self.src_pos_frames.floor() as usize;
            let frac = self.src_pos_frames - idx0 as f32;
            let idx1 = idx0 + 1;

            if idx1 >= available_src_frames {
                break;
            }

            // Map source channels -> stereo (mono duplicated; >2 uses first two).
            let l0 = Self::read_src(&q, idx0, 0.min(src_ch - 1), src_ch);
            let l1 = Self::read_src(&q, idx1, 0.min(src_ch - 1), src_ch);
            let rch = if src_ch >= 2 { 1 } else { 0 };
            let r0 = Self::read_src(&q, idx0, rch, src_ch);
            let r1 = Self::read_src(&q, idx1, rch, src_ch);

            buf[i * 2] = l0 + frac * (l1 - l0);
            buf[i * 2 + 1] = r0 + frac * (r1 - r0);

            self.src_pos_frames += ratio;
            produced_frames += 1;
        }

        // Drop whole source frames that have been consumed.
        let drop_frames = self.src_pos_frames.floor() as usize;
        let drop_samples = drop_frames.saturating_mul(src_ch).min(q.len());
        for _ in 0..drop_samples {
            q.pop_front();
        }
        self.src_pos_frames -= drop_frames as f32;

        Ok(Some(produced_frames * 2))
    }
}

impl Drop for CpalMicrophoneSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.capture_thread.take() {
            // Best effort: don't panic on join failure.
            let _ = handle.join();
        }
    }
}

/// System audio capture via CPAL WASAPI loopback (Windows only).
/// Captures the system audio output (what you hear from speakers) and makes it available
/// as an AudioSource, resampled to 48kHz stereo.
#[cfg(target_os = "windows")]
pub struct CpalSystemAudioSource {
    /// Always 48kHz stereo.
    format: AudioFormat,
    /// Interleaved f32 samples at native rate/channels from the CPAL callback.
    queue: Arc<Mutex<VecDeque<f32>>>,
    /// Native device rate.
    src_rate: u32,
    /// Native device channels.
    src_channels: usize,
    /// Resampler position in source frames.
    src_pos_frames: f32,
    /// Stops the capture thread.
    stop: Arc<AtomicBool>,
    /// Keep the capture thread alive.
    capture_thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "windows")]
impl CpalSystemAudioSource {
    /// Create a new system audio capture source at 48kHz stereo.
    /// Uses WASAPI loopback to capture system audio output.
    pub fn new_48k_stereo() -> anyhow::Result<Self> {
        use cpal::traits::HostTrait;
        
        let queue = Arc::new(Mutex::new(VecDeque::<f32>::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let (init_tx, init_rx) = std::sync::mpsc::channel::<anyhow::Result<(u32, usize)>>();
        let queue_thread = queue.clone();
        let stop_thread = stop.clone();

        let capture_thread = std::thread::spawn(move || {
            let init: anyhow::Result<(u32, usize)> = (|| {
                // Use WASAPI host for loopback support
                let host = cpal::host_from_id(cpal::HostId::Wasapi)
                    .context("WASAPI host not available")?;
                
                // Get the default output device for loopback capture
                let device = host
                    .default_output_device()
                    .context("no default output device for loopback")?;
                
                let default_cfg = device
                    .default_output_config()
                    .context("no default output config")?;

                let src_rate = default_cfg.sample_rate().0;
                let src_channels = default_cfg.channels() as usize;
                if src_channels == 0 {
                    anyhow::bail!("output device reports 0 channels");
                }

                Ok((src_rate, src_channels))
            })();

            let init_ok = init.is_ok();
            let _ = init_tx.send(init);
            if !init_ok {
                return;
            }

            // Build and keep the loopback stream alive
            let _stream = (|| -> anyhow::Result<cpal::Stream> {
                let host = cpal::host_from_id(cpal::HostId::Wasapi)
                    .context("WASAPI host not available")?;
                let device = host
                    .default_output_device()
                    .context("no default output device for loopback")?;
                let default_cfg = device
                    .default_output_config()
                    .context("no default output config")?;

                let src_rate = default_cfg.sample_rate().0;
                let src_channels = default_cfg.channels() as usize;
                // Low-latency: ~500ms buffer
                let max_samples = (src_rate as usize) * src_channels / 2;

                let err_fn = |err| tracing::warn!("cpal system audio stream error: {err}");
                
                // Build a loopback input stream from the output device
                // WASAPI allows capturing from output devices in loopback mode
                let cfg = cpal::StreamConfig {
                    channels: default_cfg.channels(),
                    sample_rate: default_cfg.sample_rate(),
                    buffer_size: cpal::BufferSize::Default,
                };

                let queue_for_callbacks = queue_thread.clone();
                let stream = match default_cfg.sample_format() {
                    cpal::SampleFormat::F32 => {
                        let queue_cb = queue_for_callbacks.clone();
                        device.build_input_stream(
                            &cfg,
                            move |data: &[f32], _| {
                                let mut q = queue_cb.lock().unwrap_or_else(|e| e.into_inner());
                                q.extend(data.iter().copied());
                                while q.len() > max_samples {
                                    q.pop_front();
                                }
                            },
                            err_fn,
                            None,
                        )?
                    }
                    cpal::SampleFormat::I16 => {
                        let queue_cb = queue_for_callbacks.clone();
                        device.build_input_stream(
                            &cfg,
                            move |data: &[i16], _| {
                                let mut q = queue_cb.lock().unwrap_or_else(|e| e.into_inner());
                                q.extend(data.iter().map(|&x| x as f32 / i16::MAX as f32));
                                while q.len() > max_samples {
                                    q.pop_front();
                                }
                            },
                            err_fn,
                            None,
                        )?
                    }
                    cpal::SampleFormat::U16 => {
                        let queue_cb = queue_for_callbacks.clone();
                        device.build_input_stream(
                            &cfg,
                            move |data: &[u16], _| {
                                let mut q = queue_cb.lock().unwrap_or_else(|e| e.into_inner());
                                q.extend(data.iter().map(|&x| (x as f32 / u16::MAX as f32) * 2.0 - 1.0));
                                while q.len() > max_samples {
                                    q.pop_front();
                                }
                            },
                            err_fn,
                            None,
                        )?
                    }
                    other => anyhow::bail!("unsupported CPAL sample format: {other:?}"),
                };
                stream.play()?;
                Ok(stream)
            })()
            .inspect_err(|err| tracing::error!("cpal system audio capture failed: {err:#}"))
            .ok();

            while !stop_thread.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
        });

        let (src_rate, src_channels) = init_rx
            .recv()
            .context("system audio init channel closed")??;

        Ok(Self {
            format: AudioFormat {
                sample_rate: 48_000,
                channel_count: 2,
            },
            src_rate,
            src_channels,
            queue,
            src_pos_frames: 0.0,
            stop,
            capture_thread: Some(capture_thread),
        })
    }

    #[inline]
    fn read_src(q: &VecDeque<f32>, frame: usize, ch: usize, src_channels: usize) -> f32 {
        q[frame * src_channels + ch]
    }
}

#[cfg(target_os = "windows")]
impl AudioSource for CpalSystemAudioSource {
    fn cloned_boxed(&self) -> Box<dyn AudioSource> {
        Box::new(Self::new_48k_stereo().expect("failed to create system audio source"))
    }

    fn format(&self) -> AudioFormat {
        self.format
    }

    fn pop_samples(&mut self, buf: &mut [f32]) -> anyhow::Result<Option<usize>> {
        // Same resampling logic as CpalMicrophoneSource
        let out_frames = buf.len() / 2;
        buf.fill(0.0);

        if out_frames == 0 {
            return Ok(Some(0));
        }

        let ratio = self.src_rate as f32 / 48_000.0;
        let src_ch = self.src_channels;

        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let available_src_frames = q.len() / src_ch;
        if available_src_frames < 2 {
            return Ok(Some(buf.len()));
        }

        let mut produced_frames = 0usize;
        for i in 0..out_frames {
            let idx0 = self.src_pos_frames.floor() as usize;
            let frac = self.src_pos_frames - idx0 as f32;
            let idx1 = idx0 + 1;

            if idx1 >= available_src_frames {
                break;
            }

            let l0 = Self::read_src(&q, idx0, 0.min(src_ch - 1), src_ch);
            let l1 = Self::read_src(&q, idx1, 0.min(src_ch - 1), src_ch);
            let rch = if src_ch >= 2 { 1 } else { 0 };
            let r0 = Self::read_src(&q, idx0, rch, src_ch);
            let r1 = Self::read_src(&q, idx1, rch, src_ch);

            buf[i * 2] = l0 + frac * (l1 - l0);
            buf[i * 2 + 1] = r0 + frac * (r1 - r0);

            self.src_pos_frames += ratio;
            produced_frames += 1;
        }

        let drop_frames = self.src_pos_frames.floor() as usize;
        let drop_samples = drop_frames.saturating_mul(src_ch).min(q.len());
        for _ in 0..drop_samples {
            q.pop_front();
        }
        self.src_pos_frames -= drop_frames as f32;

        Ok(Some(produced_frames * 2))
    }
}

#[cfg(target_os = "windows")]
impl Drop for CpalSystemAudioSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.capture_thread.take() {
            let _ = handle.join();
        }
    }
}

/// An audio source that mixes two audio sources together.
/// Both sources should be at the same sample rate and channel count (48kHz stereo).
/// Samples are added together and clamped to prevent clipping.
pub struct MixedAudioSource {
    source1: Box<dyn AudioSource>,
    source2: Box<dyn AudioSource>,
    format: AudioFormat,
    /// Temporary buffer for source2 samples
    buf2: Vec<f32>,
    /// Gain for source1 (0.0 to 1.0)
    gain1: f32,
    /// Gain for source2 (0.0 to 1.0)
    gain2: f32,
}

impl MixedAudioSource {
    /// Create a new mixed audio source from two sources.
    /// Both sources should output 48kHz stereo audio.
    pub fn new(source1: Box<dyn AudioSource>, source2: Box<dyn AudioSource>) -> Self {
        Self::with_gains(source1, source2, 1.0, 1.0)
    }

    /// Create a new mixed audio source with custom gain for each source.
    /// Gain values are typically between 0.0 and 1.0.
    pub fn with_gains(
        source1: Box<dyn AudioSource>,
        source2: Box<dyn AudioSource>,
        gain1: f32,
        gain2: f32,
    ) -> Self {
        // Use source1's format (both should be the same)
        let format = source1.format();
        Self {
            source1,
            source2,
            format,
            buf2: Vec::new(),
            gain1,
            gain2,
        }
    }

    /// Set the gain for source1.
    #[allow(dead_code)]
    pub fn set_gain1(&mut self, gain: f32) {
        self.gain1 = gain;
    }

    /// Set the gain for source2.
    #[allow(dead_code)]
    pub fn set_gain2(&mut self, gain: f32) {
        self.gain2 = gain;
    }
}

impl AudioSource for MixedAudioSource {
    fn cloned_boxed(&self) -> Box<dyn AudioSource> {
        Box::new(Self {
            source1: self.source1.cloned_boxed(),
            source2: self.source2.cloned_boxed(),
            format: self.format,
            buf2: Vec::new(),
            gain1: self.gain1,
            gain2: self.gain2,
        })
    }

    fn format(&self) -> AudioFormat {
        self.format
    }

    fn pop_samples(&mut self, buf: &mut [f32]) -> anyhow::Result<Option<usize>> {
        // Ensure buf2 is the right size
        if self.buf2.len() != buf.len() {
            self.buf2.resize(buf.len(), 0.0);
        }

        // Read from source1 into buf
        let result1 = self.source1.pop_samples(buf)?;

        // Read from source2 into buf2
        let result2 = self.source2.pop_samples(&mut self.buf2)?;

        // Mix the samples
        match (result1, result2) {
            (Some(n1), Some(n2)) => {
                let samples_to_mix = n1.min(n2).min(buf.len());
                for i in 0..samples_to_mix {
                    // Apply gains and mix
                    let mixed = buf[i] * self.gain1 + self.buf2[i] * self.gain2;
                    // Soft clamp to prevent harsh clipping
                    buf[i] = soft_clip(mixed);
                }
                // Zero out any remaining samples if source2 produced fewer
                for i in samples_to_mix..n1 {
                    buf[i] *= self.gain1;
                }
                Ok(Some(n1))
            }
            (Some(n), None) => {
                // Only source1 produced samples, apply gain
                for i in 0..n {
                    buf[i] *= self.gain1;
                }
                Ok(Some(n))
            }
            (None, Some(n)) => {
                // Only source2 produced samples, copy to buf with gain
                for i in 0..n {
                    buf[i] = self.buf2[i] * self.gain2;
                }
                Ok(Some(n))
            }
            (None, None) => Ok(None),
        }
    }
}

/// Soft clipping function to prevent harsh distortion when mixing.
/// Uses tanh-like curve for values outside [-1, 1].
#[inline]
fn soft_clip(x: f32) -> f32 {
    if x > 1.0 {
        1.0 - (-((x - 1.0) * 2.0)).exp() * 0.5
    } else if x < -1.0 {
        -1.0 + (-((-x - 1.0) * 2.0)).exp() * 0.5
    } else {
        x
    }
}

/// An AudioSource that receives decoded audio samples from a network stream.
/// Used to make remote audio available as an AudioSource for mixing.
pub struct DecodedAudioSource {
    format: AudioFormat,
    /// Queue of decoded samples
    queue: Arc<Mutex<VecDeque<f32>>>,
    /// Signals to stop the decoder thread
    stop: Arc<AtomicBool>,
    /// Decoder thread handle
    _thread_handle: Option<std::thread::JoinHandle<()>>,
    /// Rubato resampler for jitter buffering
    resampler: FftFixedOut<f32>,
    /// Intermediate buffer for resampler input
    resampler_in: Vec<Vec<f32>>,
    /// Intermediate buffer for resampler output
    resampler_out: Vec<Vec<f32>>,
    /// Ring buffer for storing resampled audio ready to be popped
    output_queue: VecDeque<f32>,
}

impl DecodedAudioSource {
    /// Create a new DecodedAudioSource from a track consumer and audio config.
    /// Spawns a decoder thread that receives and decodes audio packets.
    pub fn new<D: crate::av::AudioDecoder>(
        consumer: hang::TrackConsumer,
        config: &hang::catalog::AudioConfig,
        target_format: AudioFormat,
    ) -> anyhow::Result<Self> {
        let queue = Arc::new(Mutex::new(VecDeque::<f32>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        
        let decoder = D::new(config, target_format)?;
        
        let queue_thread = queue.clone();
        let stop_thread = stop.clone();
        
        // Initialize rubato resampler for jitter buffering
        let resampler = FftFixedOut::<f32>::new(
            target_format.sample_rate as usize,
            target_format.sample_rate as usize,
            1024, // output chunk size
            2,    // sub-chunks (batching)
            target_format.channel_count as usize,
        )?;
        let resampler_in = resampler.input_buffer_allocate(true);
        let resampler_out = resampler.output_buffer_allocate(true);

        // Spawn async task to forward packets, then spawn thread to decode
        // Buffer more packets to handle network bursts
        let (packet_tx, packet_rx) = std::sync::mpsc::sync_channel::<hang::Frame>(10);
        
        // Spawn tokio task to receive packets from the network
        let _task_handle = tokio::spawn(async move {
            let mut consumer = consumer;
            let mut packet_count = 0u64;
            loop {
                match consumer.read().await {
                    Ok(Some(frame)) => {
                        packet_count += 1;
                        if packet_count % 100 == 0 {
                            tracing::debug!("DecodedAudioSource: received {} packets", packet_count);
                        }
                        if packet_tx.send(frame).is_err() {
                            tracing::debug!("DecodedAudioSource: packet channel closed, stopping receiver");
                            break;
                        }
                    }
                    Ok(None) => {
                        tracing::debug!("DecodedAudioSource: stream ended (read None)");
                        break;
                    }
                    Err(err) => {
                        tracing::warn!("DecodedAudioSource: failed to read frame: {err:?}");
                        break;
                    }
                }
            }
            tracing::info!("DecodedAudioSource: receiver task ended after {} packets", packet_count);
        });
        
        // Spawn thread to decode packets
        let thread_handle = std::thread::spawn(move || {
            let mut decoder = decoder;
            // Buffer ~500ms max to allow jitter buffer to work
            let max_samples = (target_format.sample_rate as usize) * (target_format.channel_count as usize) / 2;
            let mut decoded_packets = 0u64;
            let mut total_samples = 0u64;
            
            while !stop_thread.load(Ordering::Relaxed) {
                // Try to receive a packet with timeout
                match packet_rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(packet) => {
                        decoded_packets += 1;
                        if let Err(err) = decoder.push_packet(packet) {
                            tracing::warn!("DecodedAudioSource: push_packet error: {err}");
                            continue;
                        }
                        match decoder.pop_samples() {
                            Ok(Some(samples)) => {
                                total_samples += samples.len() as u64;
                                let mut q = queue_thread.lock().unwrap_or_else(|e| e.into_inner());
                                let before_len = q.len();
                                q.extend(samples.iter().copied());
                                // Trim to max buffer size
                                while q.len() > max_samples {
                                    q.pop_front();
                                }
                                if decoded_packets % 100 == 0 {
                                    tracing::debug!(
                                        "DecodedAudioSource: decoded {} packets, {} samples, queue: {} -> {} samples",
                                        decoded_packets, total_samples, before_len, q.len()
                                    );
                                }
                            }
                            Ok(None) => {
                                // Decoder needs more packets
                            }
                            Err(err) => {
                                tracing::warn!("DecodedAudioSource: decoder pop_samples error: {err}");
                            }
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        // Keep looping - this is normal when waiting for packets
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        tracing::debug!("DecodedAudioSource: packet channel disconnected, stopping decoder");
                        break;
                    }
                }
            }
            tracing::info!(
                "DecodedAudioSource: decoder thread ended after {} packets, {} total samples",
                decoded_packets, total_samples
            );
        });
        
        Ok(Self {
            format: target_format,
            queue,
            stop,
            _thread_handle: Some(thread_handle),
            resampler,
            resampler_in,
            resampler_out,
            output_queue: VecDeque::new(),
        })
    }
}

impl AudioSource for DecodedAudioSource {
    fn cloned_boxed(&self) -> Box<dyn AudioSource> {
        // Cannot truly clone a network stream, return a silent source
        Box::new(SilentAudioSource { format: self.format })
    }

    fn format(&self) -> AudioFormat {
        self.format
    }

    fn pop_samples(&mut self, buf: &mut [f32]) -> anyhow::Result<Option<usize>> {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        
        let channels = self.format.channel_count as usize;
        let queue_size = q.len();
        
        // Log if queue is very low or very high (indicates potential issues)
        if queue_size < 1024 * channels {
            tracing::trace!(
                "DecodedAudioSource::pop_samples: queue low ({} samples, need {}), output_queue: {}",
                queue_size, 1024 * channels, self.output_queue.len()
            );
        } else if queue_size > 48000 * channels * 2 {
            tracing::trace!(
                "DecodedAudioSource::pop_samples: queue high ({} samples), output_queue: {}",
                queue_size, self.output_queue.len()
            );
        }
        
        // Jitter Buffer Logic:
        // Use the queue as a jitter buffer. The resampler uses a fixed 1.0 ratio
        // (no dynamic adjustment since FftFixedOut doesn't support it).
        // The queue naturally handles timing variations.
        
        // While we don't have enough output samples, try to process more input
        while self.output_queue.len() < buf.len() {
            let available_input = q.len();
            
            // Lower threshold: only need 256 samples per channel (512 total for stereo)
            // instead of 1024 per channel. This allows audio to start playing sooner.
            let min_samples = 256 * channels;
            
            if available_input < min_samples {
                break; // Wait for more data
            }

            // Get the number of frames needed for the resampler
            // Note: FftFixedOut uses a fixed ratio (1.0), so we don't adjust it
            let needed_frames = self.resampler.input_frames_next();
            let needed_samples = needed_frames * channels;
            
            // Only process if we have enough for a full resampler chunk
            if available_input < needed_samples {
                break;
            }
            
            // Copy from queue to resampler input buffer
            // De-interleave: [L, R, L, R] -> [[L, L], [R, R]]
            for i in 0..needed_frames {
                for c in 0..channels {
                    let sample = q.pop_front().unwrap_or(0.0);
                    self.resampler_in[c][i] = sample;
                }
            }
            
            // Process with fixed 1.0 ratio (no dynamic adjustment)
            let (_, out_len) = self.resampler.process_into_buffer(
                &self.resampler_in, 
                &mut self.resampler_out, 
                None
            )?;
            
            // Interleave back to output queue: [[L, L], [R, R]] -> [L, R, L, R]
            for i in 0..out_len {
                for c in 0..channels {
                    self.output_queue.push_back(self.resampler_out[c][i]);
                }
            }
        }
        
        // Now fill the output buffer from output_queue
        let to_read = buf.len().min(self.output_queue.len());
        for i in 0..to_read {
            buf[i] = self.output_queue.pop_front().unwrap_or(0.0);
        }
        
        // If output_queue is empty but we have some samples in the input queue,
        // output them directly (bypass resampler) to avoid silence
        // This is a fallback when we don't have enough for the resampler
        if to_read == 0 && q.len() >= channels {
            let direct_samples = buf.len().min(q.len());
            tracing::trace!(
                "DecodedAudioSource: using direct passthrough ({} samples from queue, bypassing resampler)",
                direct_samples
            );
            for i in 0..direct_samples {
                buf[i] = q.pop_front().unwrap_or(0.0);
            }
            // Fill rest with silence
            for i in direct_samples..buf.len() {
                buf[i] = 0.0;
            }
        } else {
            // Pad with silence (underflow)
            // Ideally we would do PLC here (repeat last sample or noise)
            for i in to_read..buf.len() {
                buf[i] = 0.0;
            }
        }
        
        Ok(Some(buf.len()))
    }
}

impl Drop for DecodedAudioSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Thread will exit on its own due to stop flag
    }
}

/// A silent audio source used as a fallback when cloning isn't possible.
struct SilentAudioSource {
    format: AudioFormat,
}

impl AudioSource for SilentAudioSource {
    fn cloned_boxed(&self) -> Box<dyn AudioSource> {
        Box::new(SilentAudioSource { format: self.format })
    }

    fn format(&self) -> AudioFormat {
        self.format
    }

    fn pop_samples(&mut self, buf: &mut [f32]) -> anyhow::Result<Option<usize>> {
        buf.fill(0.0);
        Ok(Some(buf.len()))
    }
}

// ============================================================================
// Per-Watcher Audio Pipeline (for future AEC support)
// ============================================================================

/// Per-watcher audio pipeline with slot for future AEC.
/// Each watcher has their own pipeline so we can apply individual echo cancellation.
pub struct WatcherAudioPipeline {
    /// Unique identifier for this watcher
    pub watcher_id: String,
    /// This watcher's mic audio (received from them) - stored for future AEC
    pub watcher_mic: Option<Box<dyn AudioSource>>,
    /// Base audio to send (system + publish mic)
    base_audio: Box<dyn AudioSource>,
    /// Output format
    format: AudioFormat,
    // Future: AEC processor using watcher_mic as reference
    // aec: Option<AecProcessor>,
}

impl WatcherAudioPipeline {
    /// Create a new pipeline for a watcher
    pub fn new(watcher_id: String, base_audio: Box<dyn AudioSource>) -> Self {
        let format = base_audio.format();
        Self {
            watcher_id,
            watcher_mic: None,
            base_audio,
            format,
        }
    }

    /// Set this watcher's mic audio (for future AEC)
    pub fn set_watcher_mic(&mut self, mic: Box<dyn AudioSource>) {
        self.watcher_mic = Some(mic);
    }

    /// Get the watcher ID
    pub fn id(&self) -> &str {
        &self.watcher_id
    }
}

impl AudioSource for WatcherAudioPipeline {
    fn cloned_boxed(&self) -> Box<dyn AudioSource> {
        // Clone the base audio for the new pipeline
        Box::new(Self {
            watcher_id: self.watcher_id.clone(),
            watcher_mic: None, // Can't clone network source
            base_audio: self.base_audio.cloned_boxed(),
            format: self.format,
        })
    }

    fn format(&self) -> AudioFormat {
        self.format
    }

    fn pop_samples(&mut self, buf: &mut [f32]) -> anyhow::Result<Option<usize>> {
        // Future: Apply AEC here using watcher_mic as reference
        // For now, just pass through the base audio
        // 
        // When AEC is implemented:
        // if let Some(ref mut mic) = self.watcher_mic {
        //     // Read mic samples for AEC reference
        //     // Apply AEC to remove echo from base_audio
        // }
        
        self.base_audio.pop_samples(buf)
    }
}

// ============================================================================
// Dynamic Playback Mixer (for playing all watcher mics on publisher speakers)
// ============================================================================

/// Mixes audio from dynamically added sources for local playback.
/// Used to play all watcher mic audio on publisher's speakers.
/// New sources can be added at runtime via a channel.
pub struct DynamicPlaybackMixer {
    /// Active audio sources being mixed
    sources: Vec<Box<dyn AudioSource>>,
    /// Channel to receive new sources
    add_rx: std::sync::mpsc::Receiver<Box<dyn AudioSource>>,
    /// Output format (48kHz stereo)
    format: AudioFormat,
    /// Temporary buffer for mixing
    mix_buf: Vec<f32>,
}

/// Handle to add new audio sources to the DynamicPlaybackMixer
pub struct DynamicPlaybackHandle {
    add_tx: std::sync::mpsc::SyncSender<Box<dyn AudioSource>>,
}

impl DynamicPlaybackHandle {
    /// Add a new audio source to be mixed and played
    pub fn add_source(&self, source: Box<dyn AudioSource>) -> Result<()> {
        self.add_tx.try_send(source)
            .map_err(|_| anyhow::anyhow!("playback mixer channel full or closed"))
    }
}

impl Clone for DynamicPlaybackHandle {
    fn clone(&self) -> Self {
        Self {
            add_tx: self.add_tx.clone(),
        }
    }
}

impl DynamicPlaybackMixer {
    /// Create a new dynamic playback mixer.
    /// Returns the mixer and a handle to add sources.
    pub fn new() -> (Self, DynamicPlaybackHandle) {
        let (add_tx, add_rx) = std::sync::mpsc::sync_channel(16);
        let mixer = Self {
            sources: Vec::new(),
            add_rx,
            format: AudioFormat {
                sample_rate: 48_000,
                channel_count: 2,
            },
            mix_buf: Vec::new(),
        };
        let handle = DynamicPlaybackHandle { add_tx };
        (mixer, handle)
    }

    /// Run the playback loop, continuously mixing sources and outputting to sink.
    /// This blocks and should be run in a dedicated thread.
    pub fn play_to(mut self, mut sink: impl AudioSink) {
        const INTERVAL: Duration = Duration::from_millis(20);
        let samples_per_frame = (self.format.sample_rate / 1000) * INTERVAL.as_millis() as u32;
        let buf_size = samples_per_frame as usize * self.format.channel_count as usize;
        let mut output_buf = vec![0.0f32; buf_size];
        
        loop {
            let start = Instant::now();
            
            // Check for new sources
            while let Ok(source) = self.add_rx.try_recv() {
                info!("DynamicPlaybackMixer: added new source, total: {}", self.sources.len() + 1);
                self.sources.push(source);
            }
            
            // Mix all sources
            output_buf.fill(0.0);
            
            if !self.sources.is_empty() {
                // Ensure mix_buf is the right size
                if self.mix_buf.len() != buf_size {
                    self.mix_buf.resize(buf_size, 0.0);
                }
                
                // Read from each source and mix
                let mut active_sources = 0;
                for (idx, source) in self.sources.iter_mut().enumerate() {
                    self.mix_buf.fill(0.0);
                    match source.pop_samples(&mut self.mix_buf) {
                        Ok(Some(_)) => {
                            // Check if source has any non-zero samples
                            let has_audio = self.mix_buf.iter().any(|&s| s.abs() > 0.001);
                            if has_audio {
                                active_sources += 1;
                            }
                            // Add to output
                            for (i, &sample) in self.mix_buf.iter().enumerate() {
                                output_buf[i] += sample;
                            }
                        }
                        Ok(None) => {
                            // Source returned None (end of stream)
                            tracing::debug!("DynamicPlaybackMixer: source {} returned None", idx);
                        }
                        Err(err) => {
                            tracing::warn!("DynamicPlaybackMixer: source {} error: {err}", idx);
                        }
                    }
                }
                
                // Log periodically if we have sources but no active audio
                if !self.sources.is_empty() && active_sources == 0 {
                    tracing::trace!(
                        "DynamicPlaybackMixer: {} sources but no active audio",
                        self.sources.len()
                    );
                }
                
                // Apply soft clipping if we mixed multiple sources
                if active_sources > 1 {
                    for sample in &mut output_buf {
                        *sample = soft_clip(*sample);
                    }
                }
            }
            
            // Output to sink
            if let Err(err) = sink.push_samples(&output_buf) {
                warn!("DynamicPlaybackMixer: sink error: {err}");
                break;
            }
            
            // Maintain timing
            let elapsed = start.elapsed();
            if elapsed < INTERVAL {
                std::thread::sleep(INTERVAL - elapsed);
            }
        }
    }
}

// ============================================================================
// Watcher Manager (tracks all connected watchers and their audio pipelines)
// ============================================================================

use std::collections::HashMap;

/// Manages multiple watcher connections and their audio pipelines.
/// Handles per-watcher audio for future AEC and local playback of all watcher mics.
pub struct WatcherManager {
    /// Base audio source template (system + publish mic) - cloned for each watcher
    base_audio: Box<dyn AudioSource>,
    /// Active watcher pipelines by watcher ID
    watchers: HashMap<String, WatcherAudioPipeline>,
    /// Handle to add sources to local playback mixer
    playback_handle: DynamicPlaybackHandle,
}

impl WatcherManager {
    /// Create a new WatcherManager.
    /// 
    /// # Arguments
    /// * `base_audio` - Base audio source (will be cloned for each watcher)
    /// * `playback_handle` - Handle to add watcher mics to local playback
    pub fn new(
        base_audio: Box<dyn AudioSource>,
        playback_handle: DynamicPlaybackHandle,
    ) -> Self {
        Self {
            base_audio,
            watchers: HashMap::new(),
            playback_handle,
        }
    }

    /// Add a new watcher and create their audio pipeline.
    /// Returns the audio source to publish to this watcher.
    pub fn add_watcher(&mut self, watcher_id: String) -> Box<dyn AudioSource> {
        let base_audio = self.base_audio.cloned_boxed();
        let pipeline = WatcherAudioPipeline::new(watcher_id.clone(), base_audio);
        
        // Get the output audio for this watcher
        let output = pipeline.cloned_boxed();
        
        self.watchers.insert(watcher_id.clone(), pipeline);
        info!("WatcherManager: added watcher {}, total: {}", watcher_id, self.watchers.len());
        
        output
    }

    /// Set the watcher's mic audio source.
    /// This also adds the mic to local playback so the publisher can hear them.
    pub fn set_watcher_mic(&mut self, watcher_id: &str, mic: Box<dyn AudioSource>) {
        info!("WatcherManager: set_watcher_mic called for watcher {}, format: {:?}", watcher_id, mic.format());
        
        // Add to local playback (publisher hears this watcher)
        // We must use the REAL mic source here because DecodedAudioSource cannot be cloned (clones are silent).
        // For now, we clone the SILENT version for the pipeline (AEC placeholder).
        // TODO: Implement SplitAudioSource to allow both playback and AEC.
        let mic_for_pipeline = mic.cloned_boxed();
        
        match self.playback_handle.add_source(mic) {
            Ok(()) => {
                info!("WatcherManager: successfully added mic to playback for watcher {}", watcher_id);
            }
            Err(err) => {
                warn!("WatcherManager: failed to add mic to playback for watcher {}: {err}", watcher_id);
            }
        }
        
        // Store in pipeline for future AEC
        if let Some(pipeline) = self.watchers.get_mut(watcher_id) {
            pipeline.set_watcher_mic(mic_for_pipeline);
            info!("WatcherManager: set mic for watcher {} (stored in pipeline for AEC)", watcher_id);
        } else {
            warn!("WatcherManager: watcher {} not found for mic assignment", watcher_id);
        }
    }

    /// Remove a watcher when they disconnect.
    pub fn remove_watcher(&mut self, watcher_id: &str) {
        if self.watchers.remove(watcher_id).is_some() {
            info!("WatcherManager: removed watcher {}, remaining: {}", watcher_id, self.watchers.len());
        }
    }

    /// Get the number of connected watchers.
    pub fn watcher_count(&self) -> usize {
        self.watchers.len()
    }

    /// Check if a watcher exists.
    pub fn has_watcher(&self, watcher_id: &str) -> bool {
        self.watchers.contains_key(watcher_id)
    }

    /// Get a reference to a watcher's pipeline.
    pub fn get_watcher(&self, watcher_id: &str) -> Option<&WatcherAudioPipeline> {
        self.watchers.get(watcher_id)
    }

    /// Get a mutable reference to a watcher's pipeline.
    pub fn get_watcher_mut(&mut self, watcher_id: &str) -> Option<&mut WatcherAudioPipeline> {
        self.watchers.get_mut(watcher_id)
    }
}

impl AudioSource for MicrophoneSource {
    fn cloned_boxed(&self) -> Box<dyn AudioSource> {
        Box::new(self.clone())
    }

    fn format(&self) -> AudioFormat {
        self.format
    }

    fn pop_samples(&mut self, buf: &mut [f32]) -> anyhow::Result<Option<usize>> {
        use firewheel::nodes::stream::ReadStatus;
        // Handle lock poisoning gracefully: acquire the lock even if poisoned
        let mut handle = match self.handle.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::warn!("audio input stream lock poisoned, recovering");
                poisoned.into_inner()
            }
        };
        match handle.read_interleaved(buf) {
            Some(ReadStatus::Ok) => Ok(Some(buf.len())),
            Some(ReadStatus::InputNotReady) => {
                tracing::warn!("audio input not ready");
                // Maintain pacing; still return a frame-sized buffer
                Ok(Some(buf.len()))
            }
            Some(ReadStatus::UnderflowOccurred { num_frames_read }) => {
                tracing::warn!(
                    "audio input underflow: {} frames missing",
                    buf.len() - num_frames_read
                );
                Ok(Some(buf.len()))
            }
            Some(ReadStatus::OverflowCorrected {
                num_frames_discarded,
            }) => {
                tracing::warn!("audio input overflow: {num_frames_discarded} frames discarded");
                Ok(Some(buf.len()))
            }
            None => {
                tracing::warn!("audio input stream is inactive");
                Ok(None)
            }
        }
    }
}

pub enum AudioCommand {
    OutputStream {
        config: Option<AudioConfig>,
        reply: oneshot::Sender<OutputStreamHandle>,
    },
    InputStream {
        sample_rate: u32,
        channel_count: u32,
        reply: oneshot::Sender<InputStreamHandle>,
    },
}

#[derive(Debug, Clone)]
pub struct AudioBackend {
    tx: mpsc::Sender<AudioCommand>,
}

struct AudioDriver {
    cx: FirewheelContext,
    rx: mpsc::Receiver<AudioCommand>,
}

impl AudioDriver {
    fn new(rx: mpsc::Receiver<AudioCommand>) -> Self {
        let config = FirewheelConfig {
            num_graph_inputs: ChannelCount::new(1).unwrap(),
            ..Default::default()
        };
        let mut cx = FirewheelContext::new(config);
        info!("inputs: {:?}", cx.available_input_devices());
        info!("outputs: {:?}", cx.available_output_devices());
        let config = CpalConfig {
            output: CpalOutputConfig {
                #[cfg(target_os = "linux")]
                device_name: Some("pipewire".to_string()),
                #[cfg(not(target_os = "linux"))]
                device_name: None,
                ..Default::default()
            },
            // NOTE: We intentionally start an output-only stream here to avoid the
            // CPAL duplex requirement that input/output devices share one hardware sample rate.
            // Microphone capture is handled separately via `CpalMicrophoneSource`.
            input: None,
        };
        cx.start_stream(config).unwrap();
        info!(
            "audio graph in: {:?}",
            cx.node_info(cx.graph_in_node_id()).map(|x| &x.info)
        );
        info!(
            "audio graph out: {:?}",
            cx.node_info(cx.graph_out_node_id()).map(|x| &x.info)
        );

        Self { cx, rx }
    }

    fn run(&mut self) {
        const INTERVAL: Duration = Duration::from_millis(10);
        loop {
            let tick = Instant::now();
            if self.recv().is_err() {
                info!("closing audio driver: command channel closed");
                break;
            }
            if let Err(e) = self.cx.update() {
                error!("audio backend error: {:?}", &e);

                // if let UpdateError::StreamStoppedUnexpectedly(_) = e {
                //     // Notify the stream node handles that the output stream has stopped.
                //     // This will automatically stop any active streams on the nodes.
                //     cx.node_state_mut::<StreamWriterState>(stream_writer_id)
                //         .unwrap()
                //         .stop_stream();
                //     cx.node_state_mut::<StreamReaderState>(stream_reader_id)
                //         .unwrap()
                //         .stop_stream();

                //     // The stream has stopped unexpectedly (i.e the user has
                //     // unplugged their headphones.)
                //     //
                //     // Typically you should start a new stream as soon as
                //     // possible to resume processing (event if it's a dummy
                //     // output device).
                //     //
                //     // In this example we just quit the application.
                //     break;
                // }
            }

            std::thread::sleep(INTERVAL.saturating_sub(tick.elapsed()));
        }
    }

    fn recv(&mut self) -> Result<(), ()> {
        loop {
            match self.rx.try_recv() {
                Err(TryRecvError::Disconnected) => {
                    info!("stopping audio thread: backend handle dropped");
                    break Err(());
                }
                Err(TryRecvError::Empty) => {
                    break Ok(());
                }
                Ok(command) => self.handle(command),
            }
        }
    }

    fn handle(&mut self, command: AudioCommand) {
        match command {
            AudioCommand::OutputStream { config, reply } => {
                match self.output_stream(config.as_ref()) {
                    Err(err) => {
                        warn!("failed to create audio output stream: {err:#}")
                    }
                    Ok(stream) => {
                        reply.send(stream).ok();
                    }
                }
            }
            AudioCommand::InputStream {
                sample_rate,
                channel_count,
                reply,
            } => match self.input_stream(sample_rate, channel_count) {
                Err(err) => warn!("failed to create audio input stream: {err:#}"),
                Ok(stream) => {
                    reply.send(stream).ok();
                }
            },
        }
    }

    fn output_stream(
        &mut self,
        config: Option<&hang::catalog::AudioConfig>,
    ) -> Result<OutputStreamHandle> {
        let channel_count = config.map(|c| c.channel_count).unwrap_or(2);
        let sample_rate = config.map(|c| c.sample_rate).unwrap_or(48_000);
        // setup stream
        let stream_writer_id = self.cx.add_node(
            StreamWriterNode,
            Some(StreamWriterConfig {
                channels: NonZeroChannelCount::new(channel_count)
                    .context("channel count may not be zero")?,
                ..Default::default()
            }),
        );
        let graph_out_node_id = self.cx.graph_out_node_id();
        let graph_out_info = self
            .cx
            .node_info(graph_out_node_id)
            .context("missing audio output node")?;
        let layout: &[(PortIdx, PortIdx)] = match (
            channel_count,
            graph_out_info.info.channel_config.num_inputs.get(),
        ) {
            (_, 0) => anyhow::bail!("audio output has no channels"),
            (0, _) => anyhow::bail!("audio stream has no channels"),
            (1, 2) => &[(0, 0), (0, 1)],
            (2, 2) => &[(0, 0), (1, 1)],
            (_, 1) => &[(0, 0)],
            _ => &[(0, 0), (1, 1)],
        };
        self.cx
            .connect(stream_writer_id, graph_out_node_id, layout, false)
            .unwrap();
        let output_stream_sample_rate = self.cx.stream_info().unwrap().sample_rate;
        info!(
            "Audio output: app_rate={}, hardware_rate={:?}",
            sample_rate, output_stream_sample_rate
        );
        let event = self
            .cx
            .node_state_mut::<StreamWriterState>(stream_writer_id)
            .unwrap()
            .start_stream(
                sample_rate.try_into().unwrap(),
                output_stream_sample_rate,
                ResamplingChannelConfig {
                    capacity_seconds: 0.1,    // 100ms capacity (minimal buffering here)
                    latency_seconds: 0.02,    // 20ms latency (rely on DecodedAudioSource jitter buffer)
                    ..Default::default()
                },
            )
            .unwrap();
        info!("started output stream");
        self.cx.queue_event_for(stream_writer_id, event.into());
        // Wrap the handles in an `Arc<Mutex<T>>>` so that we can send them to other threads.
        let handle = self
            .cx
            .node_state::<StreamWriterState>(stream_writer_id)
            .unwrap()
            .handle();
        Ok(Arc::new(handle))
    }

    fn input_stream(&mut self, sample_rate: u32, channel_count: u32) -> Result<InputStreamHandle> {
        // Setup stream reader node
        let stream_reader_id = self.cx.add_node(
            StreamReaderNode,
            Some(StreamReaderConfig {
                channels: NonZeroChannelCount::new(channel_count)
                    .context("channel count may not be zero")?,
                ..Default::default()
            }),
        );
        let graph_in_node_id = self.cx.graph_in_node_id();
        let graph_in_info = self
            .cx
            .node_info(graph_in_node_id)
            .context("missing audio input node")?;

        let layout: &[(PortIdx, PortIdx)] = match (
            graph_in_info.info.channel_config.num_outputs.get(),
            channel_count,
        ) {
            (0, _) => anyhow::bail!("audio input has no channels"),
            (1, 2) => &[(0, 0), (0, 1)],
            (2, 2) => &[(0, 0), (1, 1)],
            (_, 1) => &[(0, 0)],
            _ => &[(0, 0), (1, 1)],
        };
        self.cx
            .connect(graph_in_node_id, stream_reader_id, layout, false)
            .unwrap();

        let input_stream_sample_rate = self.cx.stream_info().unwrap().sample_rate;
        info!(
            "Audio input: app_rate={}, hardware_rate={:?}",
            sample_rate, input_stream_sample_rate
        );
        let event = self
            .cx
            .node_state_mut::<StreamReaderState>(stream_reader_id)
            .unwrap()
            .start_stream(
                sample_rate.try_into().unwrap(),
                input_stream_sample_rate,
                ResamplingChannelConfig {
                    capacity_seconds: 0.1,    // Allow up to 100ms buffer
                    latency_seconds: 0.03,    // 30ms latency target for capture
                    ..Default::default()
                },
            )
            .unwrap();
        self.cx.queue_event_for(stream_reader_id, event.into());

        let handle = self
            .cx
            .node_state::<StreamReaderState>(stream_reader_id)
            .unwrap()
            .handle();
        Ok(Arc::new(handle))
    }
}

impl AudioBackend {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(32);
        let _handle = std::thread::spawn(move || AudioDriver::new(rx).run());
        Self { tx }
    }

    /// Convenience: produce a default microphone audio source (48kHz stereo).
    /// Uses a blocking call to initialize the input stream synchronously.
    pub async fn default_microphone(&self) -> anyhow::Result<Box<dyn AudioSource>> {
        // Note: CPAL input stream creation is synchronous; keep async API for callers.
        Ok(Box::new(CpalMicrophoneSource::new_48k_stereo()?))
    }

    /// Capture system audio output (what you hear from speakers) at 48kHz stereo.
    /// Uses WASAPI loopback on Windows. Returns an error on other platforms.
    #[cfg(target_os = "windows")]
    pub async fn system_audio(&self) -> anyhow::Result<Box<dyn AudioSource>> {
        Ok(Box::new(CpalSystemAudioSource::new_48k_stereo()?))
    }

    /// Capture system audio output - not available on this platform.
    #[cfg(not(target_os = "windows"))]
    pub async fn system_audio(&self) -> anyhow::Result<Box<dyn AudioSource>> {
        anyhow::bail!("System audio capture is only available on Windows")
    }

    pub async fn default_speaker(&self) -> anyhow::Result<OutputControl> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx
            .send(AudioCommand::OutputStream {
                config: None,
                reply,
            })
            .await?;
        let handle = reply_rx.await?;
        Ok(OutputControl::new(handle))
    }

    pub async fn output_stream(&self, config: AudioConfig) -> Result<OutputStreamHandle> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx
            .send(AudioCommand::OutputStream {
                config: Some(config),
                reply,
            })
            .await?;
        let handle = reply_rx.await?;
        Ok(handle)
    }

    #[allow(unused)]
    pub fn blocking_output_stream(&self, config: AudioConfig) -> Result<OutputStreamHandle> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx.blocking_send(AudioCommand::OutputStream {
            config: Some(config),
            reply,
        })?;
        let handle = reply_rx.blocking_recv()?;
        Ok(handle)
    }

    #[allow(unused)]
    pub async fn input_stream(&self, sample_rate: u32, channels: u32) -> Result<InputStreamHandle> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx
            .send(AudioCommand::InputStream {
                sample_rate,
                channel_count: channels,
                reply,
            })
            .await?;
        let handle = reply_rx.await?;
        Ok(handle)
    }

    pub fn blocking_input_stream(
        &self,
        sample_rate: u32,
        channels: u32,
    ) -> Result<InputStreamHandle> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx.blocking_send(AudioCommand::InputStream {
            sample_rate,
            channel_count: channels,
            reply,
        })?;
        let handle = reply_rx.blocking_recv()?;
        Ok(handle)
    }
}
