//! Native AniWorld downloader.
//!
//! The crate is split along the pipeline: [`scrape`] turns site HTML into
//! [`model::Series`] / [`model::Episode`] values, [`extract`] resolves hoster
//! pages into playlists, [`hls`] parses those playlists and [`download`] streams
//! them with bounded concurrency before [`mux`] remuxes the result.

pub mod cli;
pub mod config;
pub mod download;
pub mod extract;
pub mod hls;
pub mod metrics;
pub mod model;
pub mod mux;
pub mod naming;
pub mod net;
pub mod numbering;
pub mod scrape;
pub mod tui;
pub mod web;
