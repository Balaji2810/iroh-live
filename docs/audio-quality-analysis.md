# Audio/Video Quality Analysis & Recommendations

*Generated: 2026-01-03*

## Analysis of the Logs

The logs show a **classic real-time audio streaming instability pattern**:

1. **Frequent jitter buffer restarts** - "starting playback with 3 frames buffered" appearing repeatedly indicates the buffer is draining completely and having to re-buffer
2. **Stream writer underflows** - The audio output device is starving for data
3. **Channel backpressure** - Network packets are arriving faster than they can be processed
4. **Timing drift** - The audio loop is falling behind by 50-230ms

### Root Causes

| Issue | Cause |
|-------|-------|
| Frequent buffer restarts | `min_buffer_size` of 3 frames (~60ms) is too aggressive for real-world network conditions |
| Underflows | The 10ms polling loop in `subscribe.rs` can't keep up when CPU load spikes |
| Backpressure | The decode → jitter buffer → output pipeline has bottlenecks |
| Timing drift | Windows thread scheduling is imprecise; the busy-wait helps but isn't perfect |

---

## Recommended Improvements

### 1. Increase Jitter Buffer Depth

Current buffer is 60ms target with 3-frame minimum. For stable playback over WAN:

```rust
const INITIAL_BUFFER_MS: u32 = 250; // Increase from 150ms
const MIN_BUFFER_FRAMES: usize = 6;  // Increase from 3
```

### 2. Implement Proper NetEQ Integration

`neteq = "0.8.0"` is already in dependencies! NetEQ is the industry-standard jitter buffer from WebRTC that provides:
- **Packet Loss Concealment (PLC)** - Uses the Opus decoder's built-in PLC
- **Adaptive buffering** - Dynamically adjusts to network conditions
- **Time-stretching** - Smoothly accelerates/decelerates audio to catch up without pitch changes

### 3. Use a Clock-Sync Mechanism

Current timing uses `thread::sleep` which is unreliable. Consider:
- **WASAPI exclusive mode** on Windows for lower latency
- **Audio clock synchronization** - Sync to the audio device's clock, not system time

---

## Missing Pieces Compared to GMeet/Parsec

| Feature | Current Implementation | GMeet/Parsec |
|---------|------------------------|--------------|
| **Congestion Control** | ❌ None | ✅ BBR/GCC (Google Congestion Control) |
| **Forward Error Correction (FEC)** | ❌ None | ✅ Opus in-band FEC + RED |
| **Bandwidth Estimation** | ❌ None | ✅ Real-time bandwidth probing |
| **Adaptive Bitrate (ABR)** | ❌ None | ✅ Dynamic quality switching |
| **Network Statistics** | ⚠️ Basic | ✅ RTT, jitter, packet loss metrics |
| **Audio Codec** | ✅ Opus | ✅ Opus |
| **Video Codec** | ✅ H.264 | ✅ H.264/VP9/AV1 + SVC |
| **Simulcast** | ⚠️ Fixed renditions | ✅ Dynamic layer switching |
| **Packet Pacing** | ❌ None | ✅ Smooth packet sending |
| **NACK/RTX** | ❌ None | ✅ Selective retransmission |
| **PLI/FIR** | ❌ None | ✅ Keyframe requests |

---

## Crates/Technologies to Investigate

### For Audio Quality

| Crate | Purpose |
|-------|---------|
| [`neteq`](https://crates.io/crates/neteq) | ✅ Already in deps - WebRTC's adaptive jitter buffer |
| [`rubato`](https://crates.io/crates/rubato) | ✅ Already in deps - High-quality async resampling |
| [`webrtc-audio-processing`](https://crates.io/crates/webrtc-audio-processing) | ✅ Already in deps - AEC, noise suppression, AGC |

### For Video Quality

| Crate | Purpose |
|-------|---------|
| [`gstreamer`](https://crates.io/crates/gstreamer) | Full media pipeline with hardware accel |
| [`openh264`](https://crates.io/crates/openh264) | Software H.264 with SVC support |

### For Network Quality

| Technology | Purpose |
|------------|---------|
| **QUIC** (via `iroh`) | ✅ Already using - provides reliability |
| **moq-transport** | Media-over-QUIC protocol (via `moq-lite`) |
| **BBR congestion control** | Better than CUBIC for real-time |
| **GCC (Google Congestion Control)** | Designed for WebRTC - no Rust crate exists yet |

### For Parsec-like Remote Desktop

| Feature | Implementation |
|---------|----------------|
| **Input capture** | [`enigo`](https://crates.io/crates/enigo), [`rdev`](https://crates.io/crates/rdev) |
| **Low-latency encoding** | NVENC/QSV/VCE hardware encoders |
| **Cursor streaming** | Separate low-latency cursor channel |
| **Input prediction** | Client-side input buffering |

---

## Priority Recommendations

### Short-term (immediately)
- Increase buffer sizes in `adaptive.rs`
- Integrate `neteq` properly (already in dependencies!)
- Add RTT/jitter metrics collection

### Medium-term
- Implement Forward Error Correction for audio
- Add bandwidth estimation
- Implement NACK/retransmission for critical video frames

### Long-term
- Implement Google Congestion Control (GCC)
- Add adaptive bitrate switching based on network conditions
- Consider SVC for video to enable smooth quality transitions

