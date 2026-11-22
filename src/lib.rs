//! PESM Coaching Zone: reads live match stats from Football Life 2027 (FL_2027.exe)
//! and serves them to phones on the home network.
//!
//! The reading parts (`process`, `stats`, `pesdb`, `live`) are usable as a library.

pub mod cpk;
pub mod images;
pub mod lineup;
pub mod live;
pub mod pesdb;
pub mod process;
pub mod stats;
