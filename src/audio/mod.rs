//! Audio pipeline: camera G.711 (PCMU) → AAC-LC → FLV audio tags.
//!
//! The camera streams microphone audio as G.711 μ-law over RTSP; the
//! legacy Windows plugin transcoded it to AAC for `flv.js`. This
//! pipeline does the same, entirely in Rust:
//!
//! ```text
//! RTP track2 (G.711 μ-law, 8 kHz mono)
//!     → g711.rs    μ-law → i16 PCM
//!     → aac.rs     AAC-LC encode (8 kHz, 32 kbps, mono, no resample)
//!     → flv.rs     FLV audio tags (SoundFormat=10) + AAC sequence header
//! ```
//!
//! Design decisions: encode at the camera's native 8 kHz (no resample —
//! `flv.js`/`WebAudio` upsamples to the output device rate) and use a
//! pure-Rust encoder (`oxideav-aac`) to keep the binary self-contained.

pub mod aac;
pub mod flv;
pub mod g711;
pub mod transcode;
