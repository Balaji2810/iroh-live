use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;
use iroh::{Endpoint, SecretKey, protocol::Router};
use iroh_live::{
    Live,
    audio::AudioBackend,
    av::{AudioPreset, VideoPreset},
    capture::ScreenCapturer,
    ffmpeg::{H264Encoder, OpusEncoder},
    publish::{AudioRenditions, PublishBroadcast, VideoRenditions},
    ticket::LiveTicket,
};
use n0_error::{Result, anyerr};

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // Start eframe
    eframe::run_native(
        "IrohLive Publisher",
        eframe::NativeOptions::default(),
        Box::new(move |cc| {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();

            let app = PublisherApp::new(&cc.egui_ctx, rt);
            Ok(Box::new(app))
        }),
    )
    .map_err(|err| anyerr!("eframe failed: {err:#}"))
}

struct PublisherApp {
    rt: tokio::runtime::Runtime,
    state: Arc<Mutex<PublisherState>>,
    selected_fps: u32,
    ticket_string: String,
    status_message: String,
}

struct PublisherState {
    _endpoint: Option<Endpoint>,
    _router: Option<Router>,
    _live: Option<Live>,
    broadcast: Option<PublishBroadcast>,
    _audio_ctx: Option<AudioBackend>,
    is_publishing: bool,
}

impl PublisherApp {
    fn new(_ctx: &egui::Context, rt: tokio::runtime::Runtime) -> Self {
        Self {
            rt,
            state: Arc::new(Mutex::new(PublisherState {
                _endpoint: None,
                _router: None,
                _live: None,
                broadcast: None,
                _audio_ctx: None,
                is_publishing: false,
            })),
            selected_fps: 30,
            ticket_string: String::new(),
            status_message: "Ready to publish".to_string(),
        }
    }

    fn start_publishing(&mut self) {
        let fps = self.selected_fps;
        let state = self.state.clone();

        self.status_message = format!("Starting publisher at {} FPS...", fps);

        let result = self.rt.block_on(async move {
            // Initialize screen capturer
            let screen = ScreenCapturer::new()?;

            // Setup audio backend
            let audio_ctx = AudioBackend::new();

            // Setup iroh and iroh-live
            let endpoint = Endpoint::builder()
                .secret_key(secret_key_from_env()?)
                .bind()
                .await?;

            let live = Live::new(endpoint.clone());
            let router = Router::builder(endpoint.clone())
                .accept(iroh_live::ALPN, live.protocol_handler())
                .spawn();

            // Create a publish broadcast
            let mut broadcast = PublishBroadcast::new();

            // Capture audio
            let mic = audio_ctx.default_microphone().await?;
            let audio = AudioRenditions::new::<OpusEncoder>(mic, [AudioPreset::Hq]);
            broadcast.set_audio(Some(audio))?;

            // Setup video with custom FPS
            let presets = vec![VideoPreset::P180, VideoPreset::P360, VideoPreset::P720, VideoPreset::P1080];
            let presets_with_fps = presets.iter()
                .map(|preset| preset.with_fps(fps))
                .collect::<Vec<_>>();
            let video = VideoRenditions::new_with_fps::<H264Encoder>(screen, presets_with_fps);
            broadcast.set_video(Some(video))?;

            // Publish under the name "hello"
            let name = "hello";
            live.publish(name, broadcast.producer()).await?;

            // Create a ticket string
            let ticket = LiveTicket::new(router.endpoint().id(), name);
            let ticket_str = ticket.serialize();

            // Store state
            {
                let mut s = state.lock().unwrap();
                s._endpoint = Some(endpoint);
                s._router = Some(router);
                s._live = Some(live);
                s.broadcast = Some(broadcast);
                s._audio_ctx = Some(audio_ctx);
                s.is_publishing = true;
            }

            n0_error::Ok(ticket_str)
        });

        match result {
            Ok(ticket) => {
                self.ticket_string = ticket;
                self.status_message = format!("Publishing at {} FPS", fps);
            }
            Err(e) => {
                self.status_message = format!("Error: {}", e);
            }
        }
    }

    fn stop_publishing(&mut self) {
        let state = self.state.clone();

        self.rt.block_on(async move {
            let mut s = state.lock().unwrap();
            if let Some(live) = s._live.take() {
                live.shutdown();
            }
            if let Some(router) = s._router.take() {
                let _ = router.shutdown().await;
            }
            if let Some(endpoint) = s._endpoint.take() {
                endpoint.close().await;
            }
            s.broadcast = None;
            s._audio_ctx = None;
            s.is_publishing = false;
        });

        self.ticket_string.clear();
        self.status_message = "Stopped publishing".to_string();
    }

    fn is_publishing(&self) -> bool {
        self.state.lock().unwrap().is_publishing
    }
}

impl eframe::App for PublisherApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(16)); // ~60fps UI refresh

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("IrohLive Publisher");
            ui.add_space(20.0);

            ui.horizontal(|ui| {
                ui.label("Frame Rate:");
                ui.add_enabled_ui(!self.is_publishing(), |ui| {
                    egui::ComboBox::from_label("")
                        .selected_text(format!("{} FPS", self.selected_fps))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.selected_fps, 15, "15 FPS");
                            ui.selectable_value(&mut self.selected_fps, 30, "30 FPS");
                            ui.selectable_value(&mut self.selected_fps, 60, "60 FPS");
                        });
                });
            });

            ui.add_space(10.0);

            ui.horizontal(|ui| {
                if !self.is_publishing() {
                    if ui.button("Start Publishing").clicked() {
                        self.start_publishing();
                    }
                } else {
                    if ui.button("Stop Publishing").clicked() {
                        self.stop_publishing();
                    }
                }
            });

            ui.add_space(20.0);

            ui.label(format!("Status: {}", self.status_message));

            if !self.ticket_string.is_empty() {
                ui.add_space(10.0);
                ui.label("Ticket (share this with viewers):");
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.ticket_string.as_str());
                    if ui.button("Copy").clicked() {
                        ui.ctx().copy_text(self.ticket_string.clone());
                    }
                });
            }

            ui.add_space(20.0);
            ui.separator();
            ui.add_space(10.0);

            ui.label("Instructions:");
            ui.label("1. Select desired frame rate (15, 30, or 60 FPS)");
            ui.label("2. Click 'Start Publishing'");
            ui.label("3. Copy the ticket and share with viewers");
            ui.label("4. Viewers can watch using: cargo run --example watch -- <ticket>");
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.is_publishing() {
            self.stop_publishing();
        }
    }
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
