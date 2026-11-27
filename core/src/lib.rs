//! tinyrtc core: the half of a call that is the same on every platform. The C face is
//! `include/tinyrtc.h`; the README of the repo says who does what.

pub mod audio;
pub mod connection;
pub mod context;
pub mod ffi;
pub mod h264;
pub mod profile;
pub mod sdp;
pub mod stun;
