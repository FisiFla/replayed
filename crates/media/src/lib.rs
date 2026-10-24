//! replayed media drivers: locating ffmpeg and performing lossless edits.

// Test bodies may unwrap and may chunk by a constant; production code may do neither.
// Scoped to the test build of this crate, so it cannot hide either in shipped code.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::chunks_exact_to_as_chunks))]

pub use binaries::sidecar_command;
pub mod binaries;
pub mod edit;
pub mod export;
pub mod fragments;
pub mod probe;

pub use binaries::{wait_with_deadline, FfmpegBinaries};
pub use fragments::{Fragment, FragmentSplitter};
pub use probe::{
    ffmpeg_reason, smoke_test_encoder, stream_layout, AvDrift, MediaInfo, StreamLayout, StreamShape,
};
