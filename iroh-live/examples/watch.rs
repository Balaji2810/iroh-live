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

            let app = App::new(
                cc.egui_ctx.clone(),
                video,
                audio_ctx,
                audio,
                broadcast,
                StatsSmoother::new(),
                endpoint,
                session,
                rt,
                30,
            );
            Ok(Box::new(app))
        }),
    )
    .map_err(|err| anyerr!("eframe failed: {err:#}"))
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ScalingMode {
    Original,
    Adaptive,
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
    selected_res: String,
    scaling_mode: ScalingMode,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    fn new(
        ctx: egui::Context,
        video: WatchTrack,
        audio_ctx: AudioBackend,
        audio: AudioTrack,
        broadcast: SubscribeBroadcast,
        stats: StatsSmoother,
        endpoint: Endpoint,
        session: LiveSession,
        rt: tokio::runtime::Runtime,
        target_fps: u32,
    ) -> Self {
        let current_track = video.rendition().to_string();
        let (selected_res, _) = Self::parse_track_name(&current_track);

        Self {
            video: VideoView::new(&ctx, video),
            _audio: audio,
            _audio_ctx: audio_ctx,
            endpoint,
            session,
            broadcast,
            stats,
            rt,
            target_fps,
            selected_res,
            scaling_mode: ScalingMode::Adaptive,
        }
    }

    fn parse_track_name(name: &str) -> (String, String) {
        // Expected format: video-{quality}-{fps}fps (e.g. video-best-30fps)
        // Fallback for old/simple format: video-{quality} (e.g. video-best) -> assumes 30fps
        let parts: Vec<&str> = name.split('-').collect();
        if parts.len() >= 3 {
            let quality = parts[1].to_string();
            let fps = parts[2].trim_end_matches("fps").to_string();
            (quality, fps)
        } else if parts.len() == 2 {
            (parts[1].to_string(), "30".to_string())
        } else {
            ("unknown".to_string(), "30".to_string())
        }
    }

    /// Convert quality string to display name
    fn quality_display_name(quality: &str) -> &'static str {
        match quality {
            "poor" => "Poor",
            "medium" => "Medium",
            "good" => "Good",
            "best" => "Best",
            // Legacy resolution names fallback
            "180p" => "Poor",
            "360p" => "Poor",
            "720p" => "Medium",
            "1080p" => "Good",
            "1440p" => "Best",
            _ => "Unknown",
        }
    }

    fn available_options(&self) -> Vec<(String, &'static str)> {
        // Return (quality_key, display_name) pairs in order: poor, medium, good, best
        let mut options = Vec::new();
        let quality_order = ["poor", "medium", "good", "best"];
        
        for quality in quality_order {
            for name in self.broadcast.video_renditions() {
                let (q, _) = Self::parse_track_name(&name);
                if q == quality {
                    options.push((q, Self::quality_display_name(quality)));
                    break;
                }
            }
        }
        
        // Also check for legacy resolution names
        let legacy_order = ["180p", "360p", "720p", "1080p", "1440p"];
        for res in legacy_order {
            for name in self.broadcast.video_renditions() {
                let (r, _) = Self::parse_track_name(&name);
                if r == res && !options.iter().any(|(k, _)| k == res) {
                    options.push((r.clone(), Self::quality_display_name(&r)));
                    break;
                }
            }
        }
        
        options
    }

    fn switch_track(&mut self, ctx: &egui::Context) {
        // Enter the tokio runtime context before calling watch_rendition
        let _guard = self.rt.enter();
        
        // Find any track with the selected resolution
        for name in self.broadcast.video_renditions() {
             let (res, _) = Self::parse_track_name(&name);
             if res == self.selected_res {
                 if let Ok(track) = self
                    .broadcast
                    .watch_rendition::<FfmpegVideoDecoder>(&Default::default(), &name)
                {
                    self.video = VideoView::new(ctx, track);
                    break;
                }
             }
        }
    }
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

                ui.add_sized(avail, self.video.render(ctx, avail, self.scaling_mode));

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
            let options = self.available_options();

            // Quality selector with human-readable names
            ui.label("Quality:");
            let old_res = self.selected_res.clone();
            
            // Find display name for current selection
            let current_display = Self::quality_display_name(&self.selected_res);
            
            egui::ComboBox::from_id_salt("quality")
                .selected_text(current_display)
                .show_ui(ui, |ui| {
                    for (quality_key, display_name) in &options {
                        ui.selectable_value(&mut self.selected_res, quality_key.clone(), *display_name);
                    }
                });
            
            ui.add_space(10.0);
            
            // Scaling selector
            ui.label("Scaling:");
            egui::ComboBox::from_id_salt("scaling")
                .selected_text(match self.scaling_mode {
                    ScalingMode::Original => "Original",
                    ScalingMode::Adaptive => "Adaptive",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.scaling_mode, ScalingMode::Original, "Original");
                    ui.selectable_value(&mut self.scaling_mode, ScalingMode::Adaptive, "Adaptive");
                });

            // If selection changed, try to switch track
            if old_res != self.selected_res {
                self.switch_track(ctx);
            }

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

    fn render(&mut self, ctx: &egui::Context, available_size: Vec2, scaling_mode: ScalingMode) -> egui::Image<'_> {
        let available_size: egui::Vec2 = available_size.into();
        
        // Get the current frame ONCE at the start
        let current_frame = self.track.current_frame();
        
        // Determine target size based on scaling mode
        let target_size = match scaling_mode {
            ScalingMode::Adaptive => {
                // Respect aspect ratio when adapting
                if let Some(ref frame) = current_frame {
                    let (w, h) = frame.img().dimensions();
                    let aspect = w as f32 / h as f32;
                    let avail_aspect = available_size.x / available_size.y;
                    
                    if avail_aspect > aspect {
                        // Limited by height
                        egui::vec2(available_size.y * aspect, available_size.y)
                    } else {
                        // Limited by width
                        egui::vec2(available_size.x, available_size.x / aspect)
                    }
                } else {
                    available_size
                }
            },
            ScalingMode::Original => {
                if let Some(ref frame) = current_frame {
                    let (w, h) = frame.img().dimensions();
                    let ppp = ctx.pixels_per_point();
                    egui::vec2(w as f32 / ppp, h as f32 / ppp)
                } else {
                    available_size
                }
            }
        };

        if target_size != self.size {
            self.size = target_size;
            let ppp = ctx.pixels_per_point();
            let w = (target_size.x * ppp) as u32;
            let h = (target_size.y * ppp) as u32;
            self.track.set_viewport(w, h);
        }

        // Use the already-fetched frame
        if let Some(frame) = current_frame {
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
