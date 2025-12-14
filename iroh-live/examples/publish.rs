use clap::Parser;
use iroh::{Endpoint, SecretKey, protocol::Router};
use iroh_live::{
    Live,
    audio::AudioBackend,
    av::{AudioPreset, VideoCodec, VideoPreset},
    capture::ScreenCapturer,
    ffmpeg::{H264Encoder, 
        OpusEncoder
    },
    publish::{
        AudioRenditions, 
        PublishBroadcast, VideoRenditions},
    ticket::LiveTicket,
};
use n0_error::StdResultExt;

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
        .accept(iroh_live::ALPN, live.protocol_handler())
        .spawn();

    // Create a publish broadcast.
    let mut broadcast = PublishBroadcast::new();

    // Capture audio, and encode with the cli-provided preset.
    let mic = audio_ctx.default_microphone().await?;
    let audio = AudioRenditions::new::<OpusEncoder>(mic, [cli.audio_preset]);
    broadcast.set_audio(Some(audio))?;

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

    // Wait for ctrl-c and then shutdown.
    tokio::signal::ctrl_c().await?;
    live.shutdown();
    router.shutdown().await.std_context("router shutdown")?;

    Ok(())
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
