use std::sync::{Arc, Mutex};

use clap::Parser;
use iroh::{Endpoint, EndpointId, SecretKey, protocol::Router};
use iroh_live::{
    ALPN, Live, LiveSession,
    audio::{AudioBackend, DecodedAudioSource, DynamicPlaybackMixer, MixedAudioSource, WatcherManager},
    av::{AudioFormat, AudioPreset, AudioSource, VideoCodec, VideoPreset},
    capture::ScreenCapturer,
    ffmpeg::{FfmpegAudioDecoder, H264Encoder, OpusEncoder},
    publish::{AudioRenditions, PublishBroadcast, VideoRenditions},
    subscribe::SubscribeBroadcast,
    ticket::LiveTicket,
};
use moq_lite::{BroadcastConsumer, OriginConsumer};
use n0_error::{StackResultExt, StdResultExt};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

#[tokio::main]
async fn main() -> n0_error::Result {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let screen = ScreenCapturer::new()?;

    // Setup audio backend AFTER camera
    let audio_ctx = AudioBackend::new();

    // Setup iroh and iroh-live.
    let endpoint = Endpoint::builder()
        .secret_key(secret_key_from_env()?)
        .bind()
        .await?;
    let live = Live::new(endpoint.clone());
    let router = Router::builder(endpoint)
        .accept(ALPN, live.protocol_handler())
        .spawn();

    // Create a publish broadcast.
    let mut broadcast = PublishBroadcast::new();

    // =========================================================================
    // Set up per-watcher audio infrastructure
    // =========================================================================
    
    // Create dynamic playback mixer for playing all watcher mics locally
    let (playback_mixer, playback_handle) = DynamicPlaybackMixer::new();
    
    // Start playback thread - publisher hears all watcher mics
    let speaker = audio_ctx.default_speaker().await
        .map_err(|e| n0_error::anyerr!("Failed to get speaker: {e}"))?;
    std::thread::spawn(move || {
        println!("Started local playback for watcher mics");
        playback_mixer.play_to(speaker);
    });

    // Build base audio source (system audio + publish mic)
    let base_audio = build_base_audio(&cli, &audio_ctx).await?;
    
    // Create watcher manager with the base audio (cloned for each watcher)
    let watcher_manager = Arc::new(Mutex::new(WatcherManager::new(
        base_audio.cloned_boxed(),
        playback_handle.clone(),
    )));

    // Use base audio for the main broadcast (watchers receive this)
    let audio = AudioRenditions::new::<OpusEncoder>(base_audio, [cli.audio_preset]);
    broadcast.set_audio(Some(audio))?;

    // =========================================================================
    // Handle manual watcher connection (via --watch-ticket)
    // Future: This will be replaced with automatic watcher detection
    // =========================================================================
    
    if let Some(ticket_str) = &cli.watch_ticket {
        let watcher_mic = connect_to_watcher(&live, ticket_str).await?;
        println!("Watcher mic audio added to local playback");
        
        // Store in watcher manager for future AEC (also adds to playback)
        let mut manager = watcher_manager.lock().unwrap();
        let watcher_id = "manual-watcher".to_string();
        manager.add_watcher(watcher_id.clone());
        manager.set_watcher_mic(&watcher_id, watcher_mic);
    }

    // Set max bitrate from CLI arg (in Mbps, converted to bits/sec)
    unsafe {
        std::env::set_var("IROH_MAX_BITRATE", (cli.max_bandwidth * 1_000_000).to_string());
    }

    // Get source dimensions from screen capture to preserve native aspect ratio
    use iroh_live::av::VideoSource;
    let source_format = screen.format();
    let [source_width, source_height] = source_format.dimensions;

    // Create presets with source dimensions for aspect ratio preservation
    use iroh_live::av::VideoPresetWithFps;
    let presets_with_fps = cli.video_presets.iter()
        .map(|preset| VideoPresetWithFps::with_source_dimensions(*preset, cli.fps, source_width, source_height))
        .collect::<Vec<_>>();

    // Construct the screen capturer in the capture thread to avoid COM conflicts on Windows.
    let video = VideoRenditions::new_with_fps_lazy::<H264Encoder, _>(|| Ok(screen), presets_with_fps)?;
    broadcast.set_video(Some(video))?;

    // Publish under the name "hello".
    let name = "hello";
    live.publish(name, broadcast.producer()).await?;

    // Create a ticket string and print
    let ticket = LiveTicket::new(router.endpoint().id(), name);
    println!("publishing at {ticket}");

    // =========================================================================
    // Automatically detect and subscribe to watcher mic broadcasts
    // =========================================================================
    
    const WATCH_MIC_BROADCAST: &str = "watch-mic";
    let (session_tx, mut session_rx) = mpsc::channel::<(EndpointId, OriginConsumer)>(16);
    live.register_session_callback(session_tx).await
        .map_err(|e| n0_error::anyerr!("Failed to register session callback: {e}"))?;
    
    let watcher_manager_clone = watcher_manager.clone();
    tokio::spawn(async move {
        while let Some((watcher_id, subscribe)) = session_rx.recv().await {
            info!(watcher_id=%watcher_id.fmt_short(), "New watcher connected, checking for mic broadcast");
            
            // Add timeout to prevent indefinite waiting
            let watcher_mic_result = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                subscribe_to_watcher_mic(subscribe, WATCH_MIC_BROADCAST)
            ).await;
            
            match watcher_mic_result {
                Ok(Ok(watcher_mic)) => {
                    info!(watcher_id=%watcher_id.fmt_short(), "Watcher mic audio detected and added");
                    
                    // Store in watcher manager (also adds to playback)
                    let mut manager = watcher_manager_clone.lock().unwrap();
                    let watcher_id_str = watcher_id.fmt_short().to_string();
                    manager.add_watcher(watcher_id_str.clone());
                    manager.set_watcher_mic(&watcher_id_str, watcher_mic);
                }
                Ok(Err(err)) => {
                    debug!(watcher_id=%watcher_id.fmt_short(), ?err, "Watcher is not publishing mic audio");
                }
                Err(_) => {
                    warn!(watcher_id=%watcher_id.fmt_short(), "Timeout waiting for watcher mic broadcast");
                }
            }
        }
    });

    
    // Wait for ctrl-c and then shutdown.
    tokio::signal::ctrl_c().await?;
    live.shutdown();
    router.shutdown().await.std_context("router shutdown")?;

    Ok(())
}

/// Build the base audio source (system audio + publish mic).
/// This is what gets sent to all watchers.
async fn build_base_audio(
    cli: &Cli,
    audio_ctx: &AudioBackend,
) -> n0_error::Result<Box<dyn AudioSource>> {
    // Get publish mic
    let mic = audio_ctx.default_microphone().await
        .map_err(|e| n0_error::anyerr!("Failed to get microphone: {e}"))?;

    // Try to capture system audio (Windows only)
    let system_audio: Option<Box<dyn AudioSource>> = match audio_ctx.system_audio().await {
        Ok(source) => {
            println!("System audio capture enabled");
            Some(source)
        }
        Err(err) => {
            println!("System audio capture not available: {err}");
            None
        }
    };

    // Mix microphone with system audio (if available)
    let audio: Box<dyn AudioSource> = match system_audio {
        Some(sys_audio) => {
            println!("Base audio: local mic + system audio (gain={})", cli.system_gain);
            Box::new(MixedAudioSource::with_gains(
                mic,
                sys_audio,
                1.0,
                cli.system_gain,
            ))
        }
        None => {
            println!("Base audio: local mic only");
            mic
        }
    };

    Ok(audio)
}

/// Subscribe to a watcher's mic broadcast using an OriginConsumer.
async fn subscribe_to_watcher_mic(
    mut subscribe: OriginConsumer,
    broadcast_name: &str,
) -> n0_error::Result<Box<dyn AudioSource>> {
    // Target format for all audio sources: 48kHz stereo
    let target_format = AudioFormat {
        sample_rate: 48_000,
        channel_count: 2,
    };

    // Wait for the broadcast to be announced and subscribe to it
    let broadcast_consumer = wait_for_broadcast(&mut subscribe, broadcast_name).await
        .map_err(|e| n0_error::anyerr!("Failed to subscribe to watcher mic broadcast: {e}"))?;
    
    let subscribe_broadcast = SubscribeBroadcast::new(broadcast_consumer).await?;
    
    // Get audio config from the catalog
    let audio_info = subscribe_broadcast.audio_info()
        .context("watcher is not publishing audio")?;
    let (rendition_name, audio_config) = audio_info.renditions.iter().next()
        .context("no audio renditions available")?;
    
    info!("Subscribing to watcher audio: {rendition_name}");
    
    // Create a track consumer for the audio
    let audio_track = subscribe_broadcast.subscribe_audio_track(rendition_name)?;
    
    // Create decoded audio source from watcher
    let watcher_audio = DecodedAudioSource::new::<FfmpegAudioDecoder>(
        audio_track,
        audio_config,
        target_format,
    ).map_err(|e| n0_error::anyerr!("Failed to create decoded audio source: {e}"))?;

    Ok(Box::new(watcher_audio))
}

/// Wait for a broadcast to be announced and return its consumer.
async fn wait_for_broadcast(
    subscribe: &mut OriginConsumer,
    name: &str,
) -> n0_error::Result<BroadcastConsumer> {
    // Check if broadcast is already available
    if let Some(consumer) = subscribe.consume_broadcast(name) {
        return Ok(consumer);
    }
    
    // Wait for announcement, but also periodically check consume_broadcast
    // in case the broadcast was announced before we started waiting
    loop {
        tokio::select! {
            // Check if broadcast became available (might have been announced already)
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                if let Some(consumer) = subscribe.consume_broadcast(name) {
                    debug!("Found broadcast '{}' via consume_broadcast (was already announced)", name);
                    return Ok(consumer);
                }
            }
            // Wait for new announcement
            result = subscribe.announced() => {
                let (path, consumer) = result
                    .ok_or_else(|| n0_error::anyerr!("Broadcast not announced: {}", name))?;
                debug!("Peer announced broadcast: {path}");
                if path.as_str() == name {
                    return consumer.ok_or_else(|| n0_error::anyerr!("Broadcast was closed: {}", name));
                }
            }
        }
    }
}

/// Connect to a watcher and get their mic audio source.
async fn connect_to_watcher(
    live: &Live,
    ticket_str: &str,
) -> n0_error::Result<Box<dyn AudioSource>> {
    // Target format for all audio sources: 48kHz stereo
    let target_format = AudioFormat {
        sample_rate: 48_000,
        channel_count: 2,
    };

    let ticket = LiveTicket::deserialize(ticket_str)
        .map_err(|e| n0_error::anyerr!("Failed to parse watch ticket: {e}"))?;
    
    println!("Connecting to watcher at {} ...", ticket.endpoint_id.fmt_short());
    
    let mut session: LiveSession = live.connect(ticket.endpoint_id).await?;
    println!("Connected to watcher!");
    
    let consumer = session.subscribe(&ticket.broadcast_name).await?;
    let subscribe_broadcast = SubscribeBroadcast::new(consumer).await?;
    
    // Get audio config from the catalog
    let audio_info = subscribe_broadcast.audio_info()
        .context("watcher is not publishing audio")?;
    let (rendition_name, audio_config) = audio_info.renditions.iter().next()
        .context("no audio renditions available")?;
    
    println!("Subscribing to watcher audio: {rendition_name}");
    
    // Create a track consumer for the audio
    let audio_track = subscribe_broadcast.subscribe_audio_track(rendition_name)?;
    
    // Create decoded audio source from watcher
    let watcher_audio = DecodedAudioSource::new::<FfmpegAudioDecoder>(
        audio_track,
        audio_config,
        target_format,
    ).map_err(|e| n0_error::anyerr!("Failed to create decoded audio source: {e}"))?;

    Ok(Box::new(watcher_audio))
}

#[derive(Parser, Debug)]
struct Cli {
    #[arg(long, default_value_t=VideoCodec::H264)]
    codec: VideoCodec,
    #[arg(long, value_delimiter=',', default_values_t=[VideoPreset::Poor, VideoPreset::Medium, VideoPreset::Good, VideoPreset::Best])]
    video_presets: Vec<VideoPreset>,
    #[arg(long, default_value_t=AudioPreset::Hq)]
    audio_preset: AudioPreset,
    #[arg(long, default_value_t=30, help="Frame rate (FPS) for video encoding")]
    fps: u32,
    #[arg(long, default_value_t=30, value_parser=clap::value_parser!(u32).range(5..=50), help="Maximum bandwidth in Mbps (5-50, default: 30)")]
    max_bandwidth: u32,
    #[arg(long, help="Watcher ticket to subscribe to remote microphone audio (optional, for testing)")]
    watch_ticket: Option<String>,
    #[arg(long, default_value_t=0.5, help="Gain for system audio mixed with publish mic (0.0-1.0)")]
    system_gain: f32,
}

fn secret_key_from_env() -> n0_error::Result<SecretKey> {
    Ok(match std::env::var("IROH_SECRET") {
        Ok(key) => key.parse()?,
        Err(_) => {
            let key = SecretKey::generate(&mut rand::rng());
            println!(
                "Created new secret. Reuse with IROH_SECRET={}",
                data_encoding::HEXLOWER.encode(&key.to_bytes())
            );
            key
        }
    })
}
