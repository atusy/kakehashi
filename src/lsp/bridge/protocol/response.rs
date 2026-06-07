//! Translate JSON-RPC responses from downstream servers back to host
//! coordinates via `RegionOffset` (line offset + first-line column adjust),
//! returning strongly-typed LSP values. All transformers share the signature
//! `fn(response, request_virtual_uri, host_uri, offset: &RegionOffset)` and
//! the same URI filter: keep real files (cross-file jumps), translate
//! request-virtual-URI matches, drop other virtual URIs (cross-region offsets
//! are unsafe). Used by goto definition/type_definition/implementation/declaration.

use super::jsonrpc::response_has_jsonrpc_error;
use super::translation::{RegionOffset, translate_virtual_range_to_host};
use super::virtual_uri::VirtualDocumentUri;
use tower_lsp_server::ls_types::{Location, LocationLink, Uri};

// =============================================================================
// Type-safe goto-family transformers
// =============================================================================

/// Normalize the `Location | Location[] | LocationLink[]` shapes returned by
/// goto-family endpoints (definition, type_definition, implementation, declaration)
/// into `Vec<LocationLink>`, filtering URIs in the process: keep real files
/// (cross-file jumps), translate matches on the request's virtual URI, drop
/// other virtual URIs since cross-region offsets are unsafe.
///
/// An empty filtered vec is preserved (not collapsed to `None`) so callers can
/// distinguish "searched, no results" from "search failed".
pub(crate) fn transform_goto_response_to_host(
    mut response: serde_json::Value,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
) -> Option<Vec<LocationLink>> {
    if response_has_jsonrpc_error(&response, "goto request") {
        return None;
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

/// Transform a single Location to host coordinates for goto endpoints, returning
/// `None` to filter out a cross-region virtual URI (its offsets are unsafe here).
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

/// Transform a single LocationLink to host coordinates for goto endpoints, applying
/// the region offset to all of its ranges (targetRange, targetSelectionRange,
/// originSelectionRange) and returning `None` to filter out a cross-region virtual URI.
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
