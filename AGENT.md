This repository implements live A/V streaming over Iroh QUIC with selectable codecs, backends, and multiple renditions. It contains both FFmpeg-backed and native (rav1e/rav1d + opus) paths.

Key modules
- av.rs
  - Core A/V traits and types: AudioSource/Encoder, VideoSource/Encoder, PixelFormat.
  - User-facing enums (strum): Backend (Native/Ffmpeg), VideoPreset (180p/360p/720p/1080p with dimensions()/fps()), AudioPreset (Hq/Lq), VideoCodec (H264/Av1), AudioCodec (Opus).

- video/ (FFmpeg path)
  - encoder.rs: H264 encoder and Av1FfmpegEncoder (libaom based), with with_preset helper.
  - decoder.rs: FFmpeg decoder supporting H.264 and AV1; exports DecodedFrame and DecoderContext used across the codebase.

- native/video/
  - encoder.rs (rav1e): RGB(A)→I420 via the yuv crate; resizes to preset dims and encodes. Exposes with_preset.
  - decoder.rs (rav1d): YUV→RGBA/BGRA via yuv with correct matrix/range; aligns on the shared DecodedFrame type to avoid adapters.

- native/audio/
  - encoder.rs (opus crate): with_preset for Hq/Lq (bitrate targets), interleaved f32 input.
  - decoder.rs (opus crate): decodes to f32 and pushes into Firewheel output.

- lib.rs
  - PublishBroadcast
    - set_video(source, IntoIterator<(VideoEncoder, VideoPreset)>)
    - set_audio(source, IntoIterator<(AudioEncoder, AudioPreset)>)
    - Fanout with watch channels (in-progress: video converted; audio next if desired) and encoder threads (cancel-on-drop scaffolding provided).
  - ConsumeBroadcast
    - watch_with(playback, Quality, Backend) and watch_rendition(playback, name, Backend) select native vs ffmpeg paths; DRY init.
    - listen_with(audio_backend, Quality) and jitter control.
    - video_renditions() exposes available renditions (name/codec/preset).
  - Jitter channels
    - Maintains tokio watch channels for audio/video jitter; slider in UI updates these.

- examples/publish.rs
  - Clap-driven: --backend, --codec, --video-presets, --audio-preset. Uses with_preset helpers and multi-renditions API.

- examples/watch.rs
  - Clap: ticket and --quality.
  - egui UI with overlay: rendition selector, backend selector (placeholder), FPS, latency, bandwidth, RTT, and jitter slider.
  - Latency is computed from a receiver-side origin and rounded to ms. Bandwidth/RTT smoothed via EMA.

Design notes
- yuv crate handles all RGB↔YUV conversions, avoiding ad-hoc color math. Matrix (Bt709/601) and range (Full/Limited) are respected.
- Native decoder uses the same DecodedFrame shape as the FFmpeg decoder to remove extra adapters.
- Jitter: video handled in UI for low complexity; audio jitter implemented in library via a FIFO delay.
- Threads: EncoderThread/DecoderThread wrappers (with shutdown tokens) are provided to enable cancel-on-drop behavior. Video fanout uses a watch channel to feed encoders; audio will follow similarly.

Suggested development flow
- To add a codec from enums in publish, prefer new helpers (next step): set_video_codecs/set_audio_codecs mapping (Codec, Preset) → encoder::with_preset.
- To extend stats, add a lightweight stats handle or expose the needed metrics via LiveSession.
- Keep the decode init DRY by routing watch_with through watch_rendition.

Style
- Favor expressive names (encoder, source, producer) over terse abbreviations.
- Use short comments on non-obvious code only.

