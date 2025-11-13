use iroh::{Endpoint, protocol::Router};
use iroh_live::{
    Live, PublishBroadcast, audio::AudioBackend, ffmpeg_log_init, video::CaptureSource,
};
use n0_error::StdResultExt;

#[tokio::main]
async fn main() -> n0_error::Result {
    tracing_subscriber::fmt::init();
    ffmpeg_log_init();

    let audio_ctx = AudioBackend::new();

    let endpoint = Endpoint::bind().await?;
    let live = Live::new(endpoint.clone());
    let router = Router::builder(endpoint)
        .accept(iroh_live::ALPN, live.protocol_handler())
        .spawn();

    let mut broadcast = PublishBroadcast::new("hello");
    broadcast.set_audio(audio_ctx)?;
    broadcast.set_video(CaptureSource::Camera)?;
    let ticket = live.publish(&broadcast).await?;
    println!("publishing at {ticket}");

    tokio::signal::ctrl_c().await?;
    live.shutdown();
    router.shutdown().await.std_context("router shutdown")?;

    Ok(())
}
