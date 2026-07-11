//! Tree-sitter language injection: discovering injected regions (e.g. Lua in
//! Markdown) and preparing them for highlighting and the LSP bridge.
//!
//! The surface is split by concern, mirroring `analysis/semantic/` and
//! `analysis/selection/`:
//! - [`offset`] — parsing the `#offset!` directive
//! - [`ranges`] — computing the included byte ranges fed to the injection parser
//! - [`content`] — clean-content extraction and byte→`Point` coordinate helpers
//! - [`language`] — resolving the injection language for a matched pattern
//! - [`discovery`] — finding regions and resolving them into cacheable form

mod content;
mod discovery;
mod language;
mod offset;
mod ranges;

/// Maximum recursion depth for nested injections to prevent stack overflow.
pub(crate) const MAX_INJECTION_DEPTH: usize = 10;

pub(crate) use content::{
    NATIVE_PARSE_BUDGET, byte_to_point, byte_to_point_anchored, parse_with_deadline,
    parse_with_ranges,
};
pub(crate) use discovery::{
    CacheableInjectionRegion, InjectionRegionInfo, InjectionResolver, REGION_IDENTITY_LAYER_BASE,
    ResolvedInjection, collect_all_injections, detect_injection,
};
pub(crate) use offset::{InjectionOffset, effective_offset_for_pattern};
pub(crate) use ranges::{
    compute_included_ranges, compute_included_ranges_clipped, has_combined_for_pattern,
    has_include_children_for_pattern, intersect_included_ranges, sub_select_included_ranges,
};

// parse_offset_directive_for_pattern is exposed for the selection-range
// tests (analysis/selection.rs); the injection submodules test the rest
// directly via their own cfg(test) modules.
#[cfg(test)]
pub(crate) use offset::parse_offset_directive_for_pattern;
