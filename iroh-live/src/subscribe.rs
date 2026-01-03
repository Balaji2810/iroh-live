use std::{collections::HashMap, sync::Arc, time::Duration};

use hang::{
    TrackConsumer,
    catalog::{AudioConfig, Catalog, CatalogConsumer, VideoConfig},
};
use moq_lite::{BroadcastConsumer, Track};
use n0_error::{Result, StackResultExt, StdResultExt};
use n0_future::task::AbortOnDropHandle;
use n0_watcher::{Watchable, Watcher};
use tokio::{
    sync::mpsc::{self, error::TryRecvError},
    time::Instant,
};
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::{Span, debug, error, info, info_span, trace, warn};

use crate::{
    av::{
        AudioDecoder, AudioSink, AudioSinkHandle, DecodeConfig, DecodedFrame, Decoders,
        PlaybackConfig, Quality, VideoDecoder, VideoSource,
    },
    ffmpeg::util::Rescaler,
    util::spawn_thread,
};

#[derive(derive_more::Debug, Clone)]
pub struct SubscribeBroadcast {
    broadcast_name: String,
    #[debug("BroadcastConsumer")]
    broadcast: BroadcastConsumer,
    // catalog_watcher: n0_watcher::Direct<CatalogWrapper>,
    catalog_watchable: Watchable<CatalogWrapper>,
    shutdown: CancellationToken,
    _catalog_task: Arc<AbortOnDropHandle<()>>,
}

#[derive(Debug, derive_more::PartialEq, derive_more::Eq, Default, Clone, derive_more::Deref)]
pub struct CatalogWrapper {
    #[eq(skip)]
    #[deref]
    inner: Arc<Catalog>,
    seq: usize,
}

impl CatalogWrapper {
    fn new(inner: Catalog, seq: usize) -> Self {
        Self {
            inner: Arc::new(inner),
            seq,
        }
    }

    pub fn video_renditions(&self) -> impl Iterator<Item = &str> {
        let mut renditions: Vec<_> = self
            .inner
            .video
            .as_ref()
            .iter()
            .map(|v| v.renditions.iter())
            .flatten()
            .map(|(name, config)| (name.as_str(), config.coded_width))
            .collect();
        renditions.sort_by(|a, b| a.1.cmp(&b.1));
        renditions.into_iter().map(|(name, _w)| name)
    }

    pub fn audio_renditions(&self) -> impl Iterator<Item = &str> + '_ {
        self.inner
            .audio
            .as_ref()
            .into_iter()
            .map(|v| v.renditions.iter())
            .flatten()
            .map(|(name, _config)| name.as_str())
    }
}

impl CatalogWrapper {
    pub fn into_inner(self) -> Arc<Catalog> {
        self.inner
    }
}

impl SubscribeBroadcast {
    pub async fn new(broadcast_name: String, broadcast: BroadcastConsumer) -> Result<Self> {
        let shutdown = CancellationToken::new();

        let (catalog_watchable, catalog_task) = {
            let track = broadcast.subscribe_track(&Catalog::default_track());
            let mut consumer = CatalogConsumer::new(track);
            let initial_catalog = consumer
                .next()
                .await
                .std_context("Broadcast closed before receiving catalog")?
                .context("Catalog track closed before receiving catalog")?;
            let watchable = Watchable::new(CatalogWrapper::new(initial_catalog, 0));

            let task = tokio::spawn({
                let shutdown = shutdown.clone();
                let watchable = watchable.clone();
                async move {
                    for seq in 1.. {
                        match consumer.next().await {
                            Ok(Some(catalog)) => {
                                watchable.set(CatalogWrapper::new(catalog, seq)).ok();
                            }
                            Ok(None) => {
                                debug!("subscribed broadcast catalog track ended");
                                break;
                            }
                            Err(err) => {
                                debug!("subscribed broadcast closed: {err:#}");
                                break;
                            }
                        }
                    }
                    shutdown.cancel();
                }
            });
            (watchable, task)
        };
        Ok(Self {
            broadcast_name,
            broadcast,
            catalog_watchable,
            _catalog_task: Arc::new(AbortOnDropHandle::new(catalog_task)),
            shutdown: CancellationToken::new(),
        })
    }

    pub fn broadcast_name(&self) -> &str {
        &self.broadcast_name
    }

    pub fn catalog_watcher(&mut self) -> n0_watcher::Direct<CatalogWrapper> {
        self.catalog_watchable.watch()
    }

    pub fn catalog(&self) -> CatalogWrapper {
        self.catalog_watchable.get()
    }

    pub fn watch<D: VideoDecoder>(&self) -> Result<WatchTrack> {
        self.watch_with::<D>(&Default::default(), Quality::Highest)
    }

    pub fn watch_with<D: VideoDecoder>(
        &self,
        playback_config: &DecodeConfig,
        quality: Quality,
    ) -> Result<WatchTrack> {
        let catalog = self.catalog().into_inner();
        let info = catalog.video.as_ref().context("no video published")?;
        let track_name =
            select_video_rendition(&info.renditions, quality).context("no video renditions")?;
        self.watch_rendition_inner::<D>(catalog, playback_config, &track_name)
    }

    pub fn watch_rendition<D: VideoDecoder>(
        &self,
        playback_config: &DecodeConfig,
        name: &str,
    ) -> Result<WatchTrack> {
        let catalog = self.catalog().into_inner();
        self.watch_rendition_inner::<D>(catalog, playback_config, name)
    }

    fn watch_rendition_inner<D: VideoDecoder>(
        &self,
        catalog: Arc<Catalog>,
        playback_config: &DecodeConfig,
        name: &str,
    ) -> Result<WatchTrack> {
        let video = catalog.video.as_ref().context("no video published")?;
        let config = video.renditions.get(name).context("rendition not found")?;
        let consumer = TrackConsumer::new(self.broadcast.subscribe_track(&Track {
            name: name.to_string(),
            priority: video.priority,
        }));
        let span = info_span!("videodec", %name);
        WatchTrack::from_consumer::<D>(
            name.to_string(),
            consumer,
            &config,
            playback_config,
            self.shutdown.child_token(),
            span,
        )
    }
    pub fn listen<D: AudioDecoder>(&self, output: impl AudioSink) -> Result<AudioTrack> {
        self.listen_with::<D>(Quality::Highest, output)
    }

    pub fn listen_with<D: AudioDecoder>(
        &self,
        quality: Quality,
        output: impl AudioSink,
    ) -> Result<AudioTrack> {
        let catalog = self.catalog();
        let info = catalog.audio.as_ref().context("no audio published")?;
        let track_name =
            select_audio_rendition(&info.renditions, quality).context("no audio renditions")?;
        self.listen_rendition::<D>(&track_name, output)
    }

    pub fn listen_rendition<D: AudioDecoder>(
        &self,
        name: &str,
        output: impl AudioSink,
    ) -> Result<AudioTrack> {
        let catalog = self.catalog().into_inner();
        self.listen_rendition_inner::<D>(catalog, name, output)
    }

    fn listen_rendition_inner<D: AudioDecoder>(
        &self,
        catalog: Arc<Catalog>,
        name: &str,
        output: impl AudioSink,
    ) -> Result<AudioTrack> {
        let audio = catalog.audio.as_ref().context("no video published")?;
        let config = audio.renditions.get(name).context("rendition not found")?;
        let consumer = TrackConsumer::new(self.broadcast.subscribe_track(&Track {
            name: name.to_string(),
            priority: audio.priority,
        }));
        let span = info_span!("audiodec", %name);
        AudioTrack::spawn::<D>(
            name.to_string(),
            consumer,
            config.clone(),
            output,
            self.shutdown.child_token(),
            span,
        )
    }

    pub fn closed(&self) -> impl Future<Output = ()> + 'static {
        self.broadcast.closed()
    }

    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }

    pub fn watch_and_listen<D: Decoders>(
        self,
        audio_out: impl AudioSink,
        config: PlaybackConfig,
    ) -> Result<AvRemoteTrack> {
        AvRemoteTrack::new::<D>(self, audio_out, config)
    }
}

pub(crate) fn select_rendition<T, P: ToString>(
    renditions: &HashMap<String, T>,
    order: &[P],
) -> Option<String> {
    order
        .iter()
        .map(ToString::to_string)
        .find(|k| renditions.contains_key(k.as_str()))
        .or_else(|| renditions.keys().next().cloned())
}

pub(crate) fn select_video_rendition<'a, T>(
    renditions: &'a HashMap<String, T>,
    q: Quality,
) -> Option<String> {
    use crate::av::VideoPreset::*;
    let order = match q {
        Quality::Highest => [P1080, P720, P360, P180],
        Quality::High => [P720, P360, P180, P1080],
        Quality::Mid => [P360, P180, P720, P1080],
        Quality::Low => [P180, P360, P720, P1080],
    };

    select_rendition(renditions, &order)
}

pub(crate) fn select_audio_rendition<'a, T>(
    renditions: &'a HashMap<String, T>,
    q: Quality,
) -> Option<String> {
    use crate::av::AudioPreset::*;
    let order = match q {
        Quality::Highest | Quality::High => [Hq, Lq],
        Quality::Mid | Quality::Low => [Lq, Hq],
    };
    select_rendition(renditions, &order)
}

pub struct AudioTrack {
    name: String,
    handle: Box<dyn AudioSinkHandle>,
    shutdown_token: CancellationToken,
    _task_handle: AbortOnDropHandle<()>,
    _thread_handle: std::thread::JoinHandle<()>,
}

impl AudioTrack {
    pub(crate) fn spawn<D: AudioDecoder>(
        name: String,
        consumer: TrackConsumer,
        config: AudioConfig,
        output: impl AudioSink,
        shutdown: CancellationToken,
        span: Span,
    ) -> Result<Self> {
        let _guard = span.enter();
        let (packet_tx, packet_rx) = mpsc::channel(400); // Adequate capacity for network jitter (~2.5 seconds at 10ms intervals)
        let output_format = output.format()?;
        info!(?config, "audio thread start");
        let decoder = D::new(&config, output_format)?;
        let handle = output.handle();
        let thread_name = format!("adec-{}", name);
        let thread = spawn_thread(thread_name, {
            let shutdown = shutdown.clone();
            let span = span.clone();
            move || {
                let _guard = span.enter();
                if let Err(err) = Self::run_loop(decoder, packet_rx, output, &shutdown) {
                    error!("audio decoder failed: {err:#}");
                }
                info!("audio decoder thread stop");
            }
        });
        let task = tokio::spawn(forward_frames(consumer, packet_tx));
        Ok(Self {
            name,
            handle,
            shutdown_token: shutdown,
            _task_handle: AbortOnDropHandle::new(task),
            _thread_handle: thread,
        })
    }

    pub fn stopped(&self) -> impl Future<Output = ()> + 'static {
        let shutdown_token = self.shutdown_token.clone();
        async move { shutdown_token.cancelled().await }
    }

    pub fn rendition(&self) -> &str {
        &self.name
    }

    pub(crate) fn run_loop(
        mut decoder: impl AudioDecoder,
        mut packet_rx: mpsc::Receiver<hang::Frame>,
        mut sink: impl AudioSink,
        shutdown: &CancellationToken,
    ) -> Result<()> {
        use crate::audio::adaptive::AdaptiveJitterBuffer;

        const INITIAL_BUFFER_MS: u32 = 150; // Initial target: 150ms for stable playback
        const FRAME_DURATION_MS: u32 = 20; // Typical Opus frame duration

        info!(
            "audio decoder loop starting with adaptive jitter buffer (target={}ms)",
            INITIAL_BUFFER_MS
        );

        // Create adaptive jitter buffer
        let mut jitter_buffer = AdaptiveJitterBuffer::new(INITIAL_BUFFER_MS, FRAME_DURATION_MS);
        
        // Timing management with drift recovery
        let loop_start = Instant::now();
        let mut timing_baseline = loop_start;
        let mut tick_count = 0u64;
        let mut consecutive_slow_ticks = 0u32;

        loop {
            let _tick = Instant::now();

            if shutdown.is_cancelled() {
                debug!("stop audio thread: cancelled");
                jitter_buffer.stats().log_summary();
                break;
            }

            // Receive all available packets and add to jitter buffer
            loop {
                match packet_rx.try_recv() {
                    Ok(packet) => {
                        // Decode packet immediately to get samples
                        decoder.push_packet(packet)?;
                        if let Some(samples) = decoder.pop_samples()? {
                            // Insert decoded samples into jitter buffer
                            jitter_buffer.insert(samples.to_vec());
                        }
                    }
                    Err(TryRecvError::Disconnected) => {
                        debug!("stop audio thread: packet_rx disconnected");
                        jitter_buffer.stats().log_summary();
                        return Ok(());
                    }
                    Err(TryRecvError::Empty) => {
                        break;
                    }
                }
            }

            // Get audio from jitter buffer for playback
            if !sink.is_paused() {
                // Get format to determine frame size
                let format = sink.format()?;
                // Calculate frame size for 10ms (INTERVAL)
                let frame_size = (format.sample_rate / 100) as usize * format.channel_count as usize;

                let decode_start = Instant::now();
                if let Some(samples) = jitter_buffer.get_audio(frame_size) {
                    sink.push_samples(&samples)?;
                    
                    let decode_time = decode_start.elapsed();
                    
                    // Warn if processing takes too long
                    if decode_time > Duration::from_millis(8) {
                        consecutive_slow_ticks += 1;
                        if consecutive_slow_ticks % 10 == 0 {
                            warn!(
                                "slow audio processing: {:?} (consecutive: {})",
                                decode_time, consecutive_slow_ticks
                            );
                        }
                    } else {
                        consecutive_slow_ticks = 0;
                    }
                }

                // Periodic stats logging
                if tick_count % 1000 == 0 && tick_count > 0 {
                    trace!(
                        "jitter buffer: depth={}/{}, jitter={:.1}ms",
                        jitter_buffer.buffer_depth(),
                        jitter_buffer.target_size(),
                        jitter_buffer.jitter_ms()
                    );
                }
            }

            // Precise timing loop with drift recovery
            tick_count += 1;
            let expected_time = Duration::from_millis(tick_count * 10);
            let real_time = Instant::now().duration_since(timing_baseline);
            
            // Check for significant timing drift and reset baseline if needed
            let drift = real_time.saturating_sub(expected_time);
            if drift > Duration::from_millis(50) {
                warn!("audio loop drifted behind by {:?}, resetting timing baseline", drift);
                timing_baseline = Instant::now();
                tick_count = 0;
                continue;
            }
            
            let sleep_time = expected_time.saturating_sub(real_time);

            // Hybrid sleep: coarse sleep + fine busy-wait for Windows precision
            if sleep_time > Duration::from_millis(2) {
                std::thread::sleep(sleep_time - Duration::from_millis(2));
                
                // Busy-wait for the remainder to hit precise timing
                let target = timing_baseline + expected_time;
                while Instant::now() < target {
                    std::thread::yield_now();
                }
            } else if !sleep_time.is_zero() {
                // For very short sleeps, just busy-wait
                let target = timing_baseline + expected_time;
                while Instant::now() < target {
                    std::thread::yield_now();
                }
            }
        }

        shutdown.cancel();
        Ok(())
    }

    pub fn handle(&self) -> &dyn AudioSinkHandle {
        self.handle.as_ref()
    }
}

impl Drop for AudioTrack {
    fn drop(&mut self) {
        self.shutdown_token.cancel();
    }
}

pub struct WatchTrack {
    video_frames: WatchTrackFrames,
    handle: WatchTrackHandle,
}

pub struct WatchTrackHandle {
    viewport: Watchable<(u32, u32)>,
    guard: WatchTrackGuard,
}

impl WatchTrackHandle {
    pub fn set_viewport(&self, w: u32, h: u32) {
        self.viewport.set((w, h)).ok();
    }

    pub fn rendition(&self) -> &str {
        &self.guard.rendition
    }
}

pub struct WatchTrackFrames {
    rx: mpsc::Receiver<DecodedFrame>,
}

impl WatchTrackFrames {
    pub fn current_frame(&mut self) -> Option<DecodedFrame> {
        let mut out = None;
        while let Ok(item) = self.rx.try_recv() {
            out = Some(item);
        }
        out
    }

    pub async fn next_frame(&mut self) -> Option<DecodedFrame> {
        if let Some(frame) = self.current_frame() {
            Some(frame)
        } else {
            self.rx.recv().await
        }
    }
}

struct WatchTrackGuard {
    rendition: String,
    _shutdown_token_guard: DropGuard,
    _task_handle: Option<AbortOnDropHandle<()>>,
    _thread_handle: Option<std::thread::JoinHandle<()>>,
}

impl WatchTrack {
    pub fn empty(rendition: impl ToString) -> Self {
        let (tx, rx) = mpsc::channel(1);
        let task = tokio::task::spawn(async move {
            std::future::pending::<()>().await;
            let _ = tx;
        });
        let guard = WatchTrackGuard {
            rendition: rendition.to_string(),
            _shutdown_token_guard: CancellationToken::new().drop_guard(),
            _task_handle: Some(AbortOnDropHandle::new(task)),
            _thread_handle: None,
        };
        Self {
            video_frames: WatchTrackFrames { rx },
            handle: WatchTrackHandle {
                viewport: Default::default(),
                guard,
            },
        }
    }

    pub(crate) fn from_video_source(
        rendition: String,
        shutdown: CancellationToken,
        mut source: impl VideoSource,
        decode_config: DecodeConfig,
    ) -> Self {
        let viewport = Watchable::new((1u32, 1u32));
        let (frame_tx, frame_rx) = tokio::sync::mpsc::channel::<DecodedFrame>(2);
        let thread_name = format!("vpr-{:>4}-{:>4}", source.name(), rendition);
        let thread = spawn_thread(thread_name, {
            let mut viewport = viewport.watch();
            let shutdown = shutdown.clone();
            move || {
                let fps = 30;
                let mut rescaler = Rescaler::new(decode_config.pixel_format.to_ffmpeg(), None)
                    .expect("failed to create rescaler");
                let frame_duration = Duration::from_secs_f32(1. / fps as f32);
                if let Err(err) = source.start() {
                    warn!("Video source failed to start: {err:?}");
                    return;
                }
                let start = Instant::now();
                for i in 1.. {
                    // let t = Instant::now();
                    if shutdown.is_cancelled() {
                        break;
                    }
                    if viewport.update() {
                        let (w, h) = viewport.peek();
                        rescaler.set_target_dimensions(*w, *h);
                    }
                    match source.pop_frame() {
                        Ok(Some(frame)) => {
                            // trace!(t=?t.elapsed(), "pop");
                            let frame = frame.to_ffmpeg();
                            let frame = rescaler.process(&frame).expect("rescaler failed");
                            let frame =
                                DecodedFrame::from_ffmpeg(frame, frame_duration, start.elapsed());
                            // trace!(t=?t.elapsed(), "convert");
                            let _ = frame_tx.blocking_send(frame);
                            // trace!(t=?t.elapsed(), "send");
                        }
                        Ok(None) => {}
                        Err(_) => break,
                    }
                    let expected_time = i * frame_duration;
                    let actual_time = start.elapsed();
                    if expected_time > actual_time {
                        std::thread::sleep(expected_time - actual_time);
                        // trace!(t=?t.elapsed(), slept=?(actual_time - expected_time), ?expected_time, ?actual_time, "done");
                    }
                }
                if let Err(err) = source.stop() {
                    warn!("Video source failed to stop: {err:?}");
                    return;
                }
            }
        });
        let guard = WatchTrackGuard {
            rendition,
            _shutdown_token_guard: shutdown.drop_guard(),
            _task_handle: None,
            _thread_handle: Some(thread),
        };
        WatchTrack {
            video_frames: WatchTrackFrames { rx: frame_rx },
            handle: WatchTrackHandle { viewport, guard },
        }
    }

    pub(crate) fn from_consumer<D: VideoDecoder>(
        rendition: String,
        consumer: TrackConsumer,
        config: &VideoConfig,
        playback_config: &DecodeConfig,
        shutdown: CancellationToken,
        span: Span,
    ) -> Result<Self> {
        let (packet_tx, packet_rx) = mpsc::channel(32);
        let (frame_tx, frame_rx) = mpsc::channel(32);
        let viewport = Watchable::new((1u32, 1u32));
        let viewport_watcher = viewport.watch();

        let _guard = span.enter();
        debug!(?config, "video decoder start");
        let decoder = D::new(config, playback_config)?;
        let thread_name = format!("vdec-{}", rendition);
        let thread = spawn_thread(thread_name, {
            let shutdown = shutdown.clone();
            let span = span.clone();
            move || {
                let _guard = span.enter();
                if let Err(err) =
                    Self::run_loop(&shutdown, packet_rx, frame_tx, viewport_watcher, decoder)
                {
                    error!("video decoder failed: {err:#}");
                }
                shutdown.cancel();
            }
        });
        let task = tokio::task::spawn(forward_frames(consumer, packet_tx));
        let guard = WatchTrackGuard {
            rendition,
            _shutdown_token_guard: shutdown.drop_guard(),
            _task_handle: Some(AbortOnDropHandle::new(task)),
            _thread_handle: Some(thread),
        };
        Ok(WatchTrack {
            video_frames: WatchTrackFrames { rx: frame_rx },
            handle: WatchTrackHandle { viewport, guard },
        })
    }

    pub fn split(self) -> (WatchTrackFrames, WatchTrackHandle) {
        (self.video_frames, self.handle)
    }

    pub fn set_viewport(&self, w: u32, h: u32) {
        self.handle.set_viewport(w, h);
    }

    pub fn rendition(&self) -> &str {
        self.handle.rendition()
    }

    pub fn current_frame(&mut self) -> Option<DecodedFrame> {
        self.video_frames.current_frame()
    }

    pub(crate) fn run_loop(
        shutdown: &CancellationToken,
        mut packet_rx: mpsc::Receiver<hang::Frame>,
        frame_tx: mpsc::Sender<DecodedFrame>,
        mut viewport_watcher: n0_watcher::Direct<(u32, u32)>,
        mut decoder: impl VideoDecoder,
    ) -> Result<(), anyhow::Error> {
        loop {
            if shutdown.is_cancelled() {
                break;
            }
            let Some(packet) = packet_rx.blocking_recv() else {
                break;
            };
            if viewport_watcher.update() {
                let (w, h) = viewport_watcher.peek();
                decoder.set_viewport(*w, *h);
            }
            let t = Instant::now();
            decoder
                .push_packet(packet)
                .context("failed to push packet")?;
            trace!(t=?t.elapsed(), "videodec: push_packet");
            while let Some(frame) = decoder.pop_frame().context("failed to pop frame")? {
                trace!(t=?t.elapsed(), "videodec: pop frame");
                if frame_tx.blocking_send(frame).is_err() {
                    break;
                }
                trace!(t=?t.elapsed(), "videodec: tx");
            }
        }
        Ok(())
    }
}

async fn forward_frames(mut track: hang::TrackConsumer, sender: mpsc::Sender<hang::Frame>) {
    let mut frames_sent = 0u64;
    let send_start = Instant::now();
    
    loop {
        let frame = track.read_frame().await;
        match frame {
            Ok(Some(frame)) => {
                // Track send time to detect backpressure (blocking sends take longer)
                let send_time = Instant::now();
                if sender.send(frame).await.is_err() {
                    debug!("audio packet channel closed, stopping forward_frames");
                    break;
                }
                let send_duration = send_time.elapsed();
                
                // If send took >1ms, channel is likely full (backpressure)
                if send_duration > Duration::from_millis(1) {
                    warn!("audio packet channel backpressure detected: send took {:?} (frames sent: {})", 
                          send_duration, frames_sent);
                }
                
                frames_sent += 1;
            }
            Ok(None) => {
                debug!("audio track ended, forwarded {} frames in {:?}", 
                      frames_sent, send_start.elapsed());
                break;
            }
            Err(err) => {
                warn!("failed to read frame: {err:?}");
                break;
            }
        }
    }
}

pub struct AvRemoteTrack {
    pub broadcast: SubscribeBroadcast,
    pub video: Option<WatchTrack>,
    pub audio: Option<AudioTrack>,
}

impl AvRemoteTrack {
    pub fn new<D: Decoders>(
        broadcast: SubscribeBroadcast,
        audio_out: impl AudioSink,
        config: PlaybackConfig,
    ) -> Result<Self> {
        let audio = broadcast
            .listen_with::<D::Audio>(config.quality, audio_out)
            .inspect_err(|err| tracing::warn!("no audio track: {err}"))
            .ok();
        let video = broadcast
            .watch_with::<D::Video>(&config.playback, config.quality)
            .inspect_err(|err| tracing::warn!("no video track: {err}"))
            .ok();
        Ok(Self {
            broadcast,
            // session: None,
            audio,
            video,
        })
    }
}
