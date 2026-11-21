//! Chonkstep's Darwin back end: a Wayland compositor that draws through the
//! Darwin framebuffer or a KMS-style GPU output and reads keyboards and
//! pointers from the kernel's HID queues. Nothing here is built unless the
//! `darwin-backend` feature is enabled; a Linux build never compiles it.
#![cfg(feature = "darwin-backend")]

pub use smithay;
pub mod bench;
pub mod damage;
pub mod glrender;
pub mod gpu;
pub mod hid;
mod input;
pub mod kms;
pub mod output;
pub mod pixels;
pub mod power;
pub mod presenter;
pub mod render;
pub mod scale;
pub mod scene;
pub mod server;
pub mod volume;
