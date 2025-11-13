#[cfg(test)]
mod tests {
    use super::{ALPN, Live, LivePublish};
    use iroh::protocol::Router;
    use n0_error::Result;

    #[tokio::test]
    async fn smoke() -> Result {
        let publisher = async move {
            let endpoint = iroh::Endpoint::bind().await?;
            let live = Live::new(endpoint.clone());

            let router = Router::builder(endpoint).accept(ALPN, live.clone()).spawn();

            let audio_ctx = AudioContext::new();

            let video = Video::builder("cam")
                .source(Camera::default_camera())
                .encodings([VideoProfile::H264Hd1080, VideoProfile::H264Sd360])
                .build();
            let audio = Audio::builder("mic")
                .source(audio_ctx.default_microphone())
                .build();

            let broadcast = Broadcast::builder("hello")
                .video(video)
                .audio(audio)
                .build();

            live.publish(broadcast);

            tokio::signal::ctrl_c().await?;
            router.shutdown().await?;
            n0_error::Ok(())
        };

        let watcher = async move {
            let audio_ctx = AudioContext::new();
            let endpoint = iroh::Endpoint::bind().await?;
            let live = Live::new(endpoint.clone());
            let broadcast = live.connect(ticket).await?;
            // let broadcast = session.consume(ticket.name).await?
            // broadcast.initialized().await?;

            if let Some(audio) = broadcast.audio_best() {
                // let rendition = audio.best();
                // let decoder = AudioDecoder::from_catalog(rendition);
                // let track = broadcast.
                audio_ctx.play(rendition);
            }

            if let Some(video) = broadcast.video_best() {
                gui.play(video);
            }

            n0_error::Ok(())
        };

        Ok(())
    }
}
