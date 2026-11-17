//! Product catalog, feeds, back-in-stock and price-drop alerts, recommendations.
//!
//! Not built yet. Everything this feature needs (routes, MCP tools and docs, jobs, scheduler
//! work) goes in this file, or in a `catalog/` folder replacing it, through the `Feature` trait.

use super::Feature;

pub struct Module;

impl Feature for Module {
    fn name(&self) -> &'static str {
        "catalog"
    }
}
