//! Customer hub: an on-site account area for shoppers.
//!
//! Not built yet. Everything this feature needs (routes, MCP tools and docs, jobs, scheduler
//! work) goes in this file, or in a `hub/` folder replacing it, through the `Feature` trait.

use super::Feature;

pub struct Module;

impl Feature for Module {
    fn name(&self) -> &'static str {
        "hub"
    }
}
