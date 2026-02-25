//! Response transformers for LSP bridge communication.
//!
//! This module provides type-safe functions to transform JSON-RPC responses from
//! downstream language servers back to host document coordinates by applying a
//! `RegionOffset` (line offset + first-line column adjustment) to coordinates.
//!
//! ## Function Signature Pattern
//!
//! Transform functions use the signature:
//! `fn(response, request_virtual_uri, host_uri, offset: RegionOffset)`
//!
//! They return strongly-typed LSP types instead of JSON, with URI-based filtering:
//! - Real file URIs → keep as-is (cross-file jumps)
//! - Same virtual URI as request → transform coordinates
//! - Different virtual URI → filter out (cross-region, can't transform safely)
//!
//! Examples: goto definition/type_definition/implementation/declaration

use log::warn;

use super::translation::{RegionOffset, translate_virtual_range_to_host};
use super::virtual_uri::VirtualDocumentUri;
use tower_lsp_server::ls_types::{Location, LocationLink, Uri};

// =============================================================================
// Type-safe goto-family transformers
// =============================================================================

/// Transform goto-family responses to typed Vec<LocationLink> format.
///
/// This function handles all goto-style endpoints (definition, type_definition,
/// implementation, declaration) that return Location | Location[] | LocationLink[].
///
/// All response variants are normalized to Vec<LocationLink> for internal consistency,
/// with proper URI filtering and coordinate transformation.
///
/// # URI Filtering Logic
///
/// - Real file URIs → keep as-is (cross-file jumps)
/// - Same virtual URI as request → transform coordinates
/// - Different virtual URI → filter out (cross-region, can't transform safely)
///
/// Empty arrays after filtering are preserved to distinguish "searched, found nothing"
/// from "search failed" (None).
///
/// # Arguments
/// * `response` - Raw JSON-RPC response envelope (`{"result": {...}}`)
/// * `request_virtual_uri` - The virtual URI from the request
/// * `host_uri` - The pre-parsed host URI to use in transformed responses
/// * `offset` - The region offset for coordinate translation
pub(crate) fn transform_goto_response_to_host(
    mut response: serde_json::Value,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
) -> Option<Vec<LocationLink>> {
    if let Some(error) = response.get("error") {
        warn!(target: "kakehashi::bridge", "Downstream server returned error for goto request: {}", error);
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;
    if result.is_null() {
        return None;
    }

    // The LSP spec defines GotoDefinitionResponse as: Location | Location[] | LocationLink[]
    // Normalize all formats to Vec<LocationLink> for simpler internal handling

    if result.is_object() {
        // Single Location → convert to LocationLink
        if let Ok(location) = serde_json::from_value::<Location>(result) {
            return transform_location_for_goto(location, request_virtual_uri, host_uri, offset)
                .map(|loc| vec![location_to_location_link(loc)]);
        }
    } else if result.is_array() {
        // Could be Location[] or LocationLink[]
        let arr = result.as_array()?;
        if arr.is_empty() {
            // Preserve empty arrays (semantic: "searched, found nothing")
            return Some(vec![]);
        }

        // Check if first element has "targetUri" to distinguish LocationLink from Location
        if arr.first()?.get("targetUri").is_some() {
            // LocationLink[] → use directly
            if let Ok(links) = serde_json::from_value::<Vec<LocationLink>>(result) {
                let transformed: Vec<LocationLink> = links
                    .into_iter()
                    .filter_map(|link| {
                        transform_location_link_for_goto(
                            link,
                            request_virtual_uri,
                            host_uri,
                            offset,
                        )
                    })
                    .collect();

                // Preserve empty array after filtering
                return Some(transformed);
            }
        } else {
            // Location[] → convert each to LocationLink
            if let Ok(locations) = serde_json::from_value::<Vec<Location>>(result) {
                let transformed: Vec<LocationLink> = locations
                    .into_iter()
                    .filter_map(|location| {
                        transform_location_for_goto(location, request_virtual_uri, host_uri, offset)
                            .map(location_to_location_link)
                    })
                    .collect();

                // Preserve empty array after filtering
                return Some(transformed);
            }
        }
    }

    // Failed to deserialize as any known variant
    None
}

/// Convert a Location to LocationLink format.
///
/// This is a lossless conversion - LocationLink is the more feature-rich format.
/// We set `targetSelectionRange` equal to `targetRange` since Location doesn't
/// distinguish between the full symbol range and the selection range.
fn location_to_location_link(location: Location) -> LocationLink {
    LocationLink {
        origin_selection_range: None,
        target_uri: location.uri,
        target_range: location.range,
        target_selection_range: location.range, // Use same range for selection
    }
}

/// Convert a LocationLink to Location for clients that don't support linkSupport.
///
/// Uses `target_selection_range` (the symbol name) rather than `target_range`
/// (the whole definition) for more precise navigation to the symbol itself.
pub(crate) fn location_link_to_location(link: LocationLink) -> Location {
    Location {
        uri: link.target_uri,
        range: link.target_selection_range,
    }
}

/// Transform a single Location to host coordinates for goto endpoints.
///
/// Returns `None` if the location should be filtered out (cross-region virtual URI).
///
/// # URI Filtering Logic
///
/// 1. Real file URI → preserve as-is (cross-file jump to real file) - KEEP
/// 2. Same virtual URI as request → transform using request's context - KEEP
/// 3. Different virtual URI → cross-region jump - FILTER OUT
pub(crate) fn transform_location_for_goto(
    mut location: Location,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
) -> Option<Location> {
    let uri_str = location.uri.as_str();

    // Case 1: NOT a virtual URI (real file reference) → preserve as-is
    if !VirtualDocumentUri::is_virtual_uri(uri_str) {
        return Some(location);
    }

    // Case 2: Same virtual URI as request → use request's context
    if uri_str == request_virtual_uri {
        location.uri = host_uri.clone();
        translate_virtual_range_to_host(&mut location.range, offset);
        return Some(location);
    }

    // Case 3: Different virtual URI (cross-region) → filter out
    None
}

/// Transform a single LocationLink to host coordinates for goto endpoints.
///
/// Returns `None` if the location should be filtered out (cross-region virtual URI).
///
/// All ranges (targetRange, targetSelectionRange, originSelectionRange) are in virtual
/// coordinates from the downstream server and need the region offset applied.
fn transform_location_link_for_goto(
    mut link: LocationLink,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
) -> Option<LocationLink> {
    let uri_str = link.target_uri.as_str();

    // Case 1: NOT a virtual URI (real file reference) → preserve as-is
    if !VirtualDocumentUri::is_virtual_uri(uri_str) {
        return Some(link);
    }

    // Case 2: Same virtual URI as request → use request's context
    if uri_str == request_virtual_uri {
        link.target_uri = host_uri.clone();
        translate_virtual_range_to_host(&mut link.target_range, offset);
        translate_virtual_range_to_host(&mut link.target_selection_range, offset);
        if let Some(ref mut origin_range) = link.origin_selection_range {
            translate_virtual_range_to_host(origin_range, offset);
        }
        return Some(link);
    }

    // Case 3: Different virtual URI (cross-region) → filter out
    None
}
