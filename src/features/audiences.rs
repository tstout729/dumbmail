//! Ad audience sync (Meta, Google, TikTok).
//!
//! Not built yet. Everything this feature needs (routes, MCP tools and docs, jobs, scheduler
//! work) goes in this file, or in a `audiences/` folder replacing it, through the `Feature` trait.

use super::Feature;

pub struct Module;

impl Feature for Module {
    fn name(&self) -> &'static str {
        "audiences"
    }
}
