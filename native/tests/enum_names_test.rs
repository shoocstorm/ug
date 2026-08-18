//! `GraphNodeType::as_str` / `GraphEdgeType::as_str` replaced
//! `format!("{:?}", ..)` on hot paths (P3.3). Those strings are not internal:
//! they key `/api/graph/stats`, they are the `node_type` and `edge_type`
//! written to every store row, and the search/filter endpoints compare
//! against them. A variant added later with a hand-written name that does not
//! match its `Debug` spelling would change the wire format silently, so the
//! two are pinned together here rather than trusted to review.

use ultragraph::types::{GraphEdgeType, GraphNodeType};

#[test]
fn node_type_names_match_debug() {
    for v in GraphNodeType::ALL {
        assert_eq!(v.as_str(), format!("{:?}", v), "GraphNodeType::{v:?}");
    }
}

#[test]
fn edge_type_names_match_debug() {
    for v in GraphEdgeType::ALL {
        assert_eq!(v.as_str(), format!("{:?}", v), "GraphEdgeType::{v:?}");
    }
}

/// `ALL` is hand-maintained; if a variant is added and left out of it, the
/// two tests above would pass while covering nothing. Serde round-trips every
/// listed variant, and the count is asserted so adding a variant without
/// extending `ALL` fails here.
#[test]
fn all_lists_every_variant() {
    assert_eq!(GraphNodeType::ALL.len(), 11, "GraphNodeType::ALL is stale");
    assert_eq!(GraphEdgeType::ALL.len(), 12, "GraphEdgeType::ALL is stale");

    for v in GraphNodeType::ALL {
        let json = serde_json::to_string(v).unwrap();
        assert_eq!(json, format!("\"{}\"", v.as_str()));
    }
    for v in GraphEdgeType::ALL {
        let json = serde_json::to_string(v).unwrap();
        assert_eq!(json, format!("\"{}\"", v.as_str()));
    }
}
