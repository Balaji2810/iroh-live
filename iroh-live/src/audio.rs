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

                // Bound buffer to ~2 seconds to avoid unbounded latency growth.
                let max_samples = (src_rate as usize) * src_channels * 2;

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
                let max_samples = (src_rate as usize) * src_channels * 2;

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
                    capacity_seconds: 0.15,   // Allow up to 150ms buffer
                    latency_seconds: 0.04,    // Start playback after 40ms buffered (low-latency)
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
                    capacity_seconds: 0.15,   // Allow up to 150ms buffer (low-latency)
                    latency_seconds: 0.04,    // 40ms latency target (low-latency)
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
