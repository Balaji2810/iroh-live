use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Id, Vec2};
use iroh::Endpoint;
use iroh_live::{
    Live, LiveSession,
    audio::AudioBackend,
    ffmpeg::{FfmpegAudioDecoder, FfmpegVideoDecoder, ffmpeg_log_init},
    subscribe::{AudioTrack, SubscribeBroadcast, WatchTrack},
    ticket::LiveTicket,
    util::StatsSmoother,
};
use n0_error::{Result, StackResultExt, anyerr};

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    ffmpeg_log_init();
    let ticket_str = std::env::args()
        .into_iter()
        .nth(1)
        .context("missing ticket")?;
    let ticket = LiveTicket::deserialize(&ticket_str)?;

    println!("connecting to {ticket} ...");

    // Start eframe FIRST - before any async/network code
    // This ensures winit initializes COM correctly
    eframe::run_native(
        "IrohLive",
        eframe::NativeOptions::default(),
        Box::new(move |cc| {
            // Create tokio runtime INSIDE eframe callback
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();

            // Initialize audio backend
            let audio_ctx = AudioBackend::new();

            // Now do the connection
            let (endpoint, session, broadcast, video, audio) = rt.block_on({
                let audio_ctx = audio_ctx.clone();
                async move {
                    let endpoint = Endpoint::bind().await?;
                    let live = Live::new(endpoint.clone());
                    let mut session = live.connect(ticket.endpoint_id).await?;
                    println!("connected!");
                    let consumer = session.subscribe(&ticket.broadcast_name).await?;
                    let broadcast = SubscribeBroadcast::new(consumer).await?;
                    let audio_out = audio_ctx.default_speaker().await?;
                    let audio = broadcast.listen::<FfmpegAudioDecoder>(audio_out)?;
                    let video = broadcast.watch::<FfmpegVideoDecoder>()?;
                    n0_error::Ok((endpoint, session, broadcast, video, audio))
                }
            }).expect("Failed to connect");

            let app = App {
                video: VideoView::new(&cc.egui_ctx, video),
                _audio_ctx: audio_ctx,
                _audio: audio,
                broadcast,
                stats: StatsSmoother::new(),
                endpoint,
                session,
                rt,
                target_fps: 30,
            };
            Ok(Box::new(app))
        }),
    )
    .map_err(|err| anyerr!("eframe failed: {err:#}"))
}

struct App {
    video: VideoView,
    _audio: AudioTrack,
    _audio_ctx: AudioBackend,
    endpoint: Endpoint,
    session: LiveSession,
    broadcast: SubscribeBroadcast,
    stats: StatsSmoother,
    rt: tokio::runtime::Runtime,
    target_fps: u32,
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Adjust repaint rate based on target FPS
        let repaint_interval = Duration::from_millis(1000 / self.target_fps.max(15) as u64);
        ctx.request_repaint_after(repaint_interval);

        egui::CentralPanel::default()
            .frame(egui::Frame::new().inner_margin(0.0).outer_margin(0.0))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);

                // Reserve space for the bottom overlay (approximate height)
                let mut avail = ui.available_size();
                avail.y -= 50.0; // Reserve space at bottom for overlay

                ui.add_sized(avail, self.video.render(ctx, avail));

                // Bottom horizontal overlay
                egui::Area::new(Id::new("overlay"))
                    .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -8.0])
                    .show(ctx, |ui| {
                        egui::Frame::new()
                            .fill(egui::Color32::from_rgba_unmultiplied(0, 0, 0, 180))
                            .corner_radius(3.0)
                            .inner_margin(8.0)
                            .show(ui, |ui| {
                                self.render_overlay(ctx, ui);
                            })
                    })
            });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let endpoint = self.endpoint.clone();
        self.rt.block_on(async move {
            endpoint.close().await;
        });
    }
}

impl App {
    fn render_overlay(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            // Quality selector
            let selected = self.video.track.rendition().to_owned();
            ui.label("Quality:");
            egui::ComboBox::from_id_salt("quality")
                .selected_text(selected.clone())
                .show_ui(ui, |ui| {
                    for name in self.broadcast.video_renditions() {
                        if ui.selectable_label(&selected == name, name).clicked() {
                            // Enter the tokio runtime context before calling watch_rendition
                            let _guard = self.rt.enter();
                            if let Ok(track) = self
                                .broadcast
                                .watch_rendition::<FfmpegVideoDecoder>(&Default::default(), &name)
                            {
                                self.video = VideoView::new(ctx, track);
                            }
                        }
                    }
                });

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(10.0);

            // FPS target selector
            ui.label("Target FPS:");
            egui::ComboBox::from_id_salt("fps")
                .selected_text(format!("{}", self.target_fps))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.target_fps, 15, "15");
                    ui.selectable_value(&mut self.target_fps, 30, "30");
                    ui.selectable_value(&mut self.target_fps, 60, "60");
                });

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(10.0);

            // Metrics
            let (rtt, bw) = self.stats.smoothed(|| self.session.conn().stats());
            ui.label(format!("BW: {bw}"));
            ui.add_space(10.0);
            ui.label(format!("RTT: {}ms", rtt.as_millis()));
            ui.add_space(10.0);
            ui.label(format!("FPS: {:.1}", self.video.fps()));
        });
    }
}

struct VideoView {
    track: WatchTrack,
    texture: egui::TextureHandle,
    size: egui::Vec2,
    // FPS tracking
    frame_count: u32,
    last_fps_update: Instant,
    current_fps: f32,
    last_frame_timestamp: Option<Duration>,
}

impl VideoView {
    fn new(ctx: &egui::Context, track: WatchTrack) -> Self {
        let size = egui::vec2(100., 100.);
        let color_image =
            egui::ColorImage::filled([size.x as usize, size.y as usize], Color32::BLACK);
        let texture = ctx.load_texture("video", color_image, egui::TextureOptions::default());
        Self {
            size,
            texture,
            track,
            frame_count: 0,
            last_fps_update: Instant::now(),
            current_fps: 0.0,
            last_frame_timestamp: None,
        }
    }

    fn render(&mut self, ctx: &egui::Context, available_size: Vec2) -> egui::Image<'_> {
        let available_size = available_size.into();
        if available_size != self.size {
            self.size = available_size;
            let ppp = ctx.pixels_per_point();
            let w = (available_size.x * ppp) as u32;
            let h = (available_size.y * ppp) as u32;
            self.track.set_viewport(w, h);
        }

        // Check for new frame
        if let Some(frame) = self.track.current_frame() {
            let frame_timestamp = frame.timestamp;

            // Only count if this is a new frame (different timestamp)
            if self.last_frame_timestamp != Some(frame_timestamp) {
                let (w, h) = frame.img().dimensions();
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [w as usize, h as usize],
                    frame.img().as_raw(),
                );
                self.texture.set(image, Default::default());

                // Count this unique frame
                self.frame_count += 1;
                self.last_frame_timestamp = Some(frame_timestamp);
            }
        }

        // Update FPS counter every second
        let elapsed = self.last_fps_update.elapsed();
        if elapsed >= Duration::from_secs(1) {
            self.current_fps = self.frame_count as f32 / elapsed.as_secs_f32();
            self.frame_count = 0;
            self.last_fps_update = Instant::now();
        }

        egui::Image::from_texture(&self.texture).shrink_to_fit()
    }

    fn fps(&self) -> f32 {
        self.current_fps
    }
}
