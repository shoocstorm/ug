//! `nidx.rs` — split out of `serve.rs`; see `docs/dev/REFACTOR-TRACKING.md`.

use super::snapshot::build_slim_columns;
use ultragraph::types::GraphData;

// ---------- Binary slim index (`/api/graph/nodes.bin`) ----------
//
// The JSON index above is 99.7 MB on a 500k-node graph, and 90 MB of that is
// two arrays of half a million separate strings. `JSON.parse` on it costs the
// tab 236 MB of heap before a single node is drawn — see
// `docs/dev/PERF-TUNING-JOURNEY.md` §Round 2 for the measurement.
//
// This is the same data as a flat buffer the client can take typed-array views
// over: nothing to parse, and the id/name bytes never become JS strings at all.

/// Magic + version. Bumped only for an incompatible layout change; the client
/// refuses a frame it does not recognise and falls back to the JSON index,
/// so an old page against a new server degrades rather than breaks.
pub(crate) const NIDX_MAGIC: &[u8; 8] = b"UGNIDX\0\0";
const NIDX_VERSION: u32 = 1;

/// Front-coding restart interval. Every 16th entry is stored whole, so
/// `idAt(i)` on the client reconstructs from at most 16 records — the trade
/// between blob size (a longer block shares more) and lookup cost. Measured on
/// the real `~/.ug/neo4j` ids: 21.8 MB raw → 8.4 MB at 16, 7.8 MB at 64. The
/// extra 0.6 MB is not worth quadrupling the walk.
pub(crate) const NIDX_BLOCK: usize = 16;

/// Section kinds. Numbers are wire format — append, never renumber.
mod nidx {
    pub const TYPE_IDX: u32 = 1;
    pub const FILE_IDX: u32 = 2;
    pub const START_LINE: u32 = 3;
    pub const END_LINE: u32 = 4;
    pub const DEG: u32 = 5;
    pub const BOUNDARY: u32 = 6;
    pub const CATALOG_ROOTS: u32 = 7;
    pub const ID_BLOB: u32 = 8;
    pub const ID_OFF: u32 = 9;
    pub const NAME_BLOB: u32 = 10;
    pub const NAME_OFF: u32 = 11;
    pub const ID_HASH: u32 = 12;
    pub const META: u32 = 13;
}

/// FNV-1a over the UTF-8 bytes. Duplicated in `00-preamble.js` as `fnv1a32`;
/// `nidx_hash_matches_the_client` pins the two together, because a divergence
/// here is a lookup table that silently answers "no such node".
pub(crate) fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut h: u32 = 2166136261;
    for &b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    h
}

/// Front-code `values` into a blob plus `values.len() + 1` byte offsets.
///
/// Record `i` occupies `blob[off[i]..off[i + 1]]` and is one byte of
/// shared-prefix length followed by the suffix — the suffix length is the
/// record's extent, so it needs no field of its own. Every `NIDX_BLOCK`th
/// record restarts with a shared length of 0.
pub(crate) fn front_code(values: &[&str]) -> (Vec<u8>, Vec<u32>) {
    let mut blob: Vec<u8> = Vec::with_capacity(values.len() * 24);
    let mut off: Vec<u32> = Vec::with_capacity(values.len() + 1);
    off.push(0);
    let mut prev: &[u8] = b"";
    for (i, v) in values.iter().enumerate() {
        let b = v.as_bytes();
        let shared = if i % NIDX_BLOCK == 0 {
            0
        } else {
            // Capped at 255 because the field is one byte, and at the shorter
            // of the two strings.
            let max = prev.len().min(b.len()).min(255);
            let mut k = 0;
            while k < max && prev[k] == b[k] {
                k += 1;
            }
            k
        };
        blob.push(shared as u8);
        blob.extend_from_slice(&b[shared..]);
        off.push(blob.len() as u32);
        prev = b;
    }
    (blob, off)
}

/// One section's bytes, ready to be placed in the frame.
enum Section {
    Bytes(Vec<u8>),
}

fn u32_section(v: &[u32]) -> Section {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    Section::Bytes(out)
}

fn i32_section(v: &[i64]) -> Section {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&(*x as i32).to_le_bytes());
    }
    Section::Bytes(out)
}

/// The `.bin` frame:
///
/// ```text
/// "UGNIDX\0\0"  u32 version  u32 sectionCount
/// sectionCount × (u32 kind, u32 offset, u32 len)      // offsets from frame start
/// payload, each section padded to a 4-byte boundary
/// ```
///
/// The padding is what lets the client take an `Int32Array`/`Uint32Array` view
/// straight over the response buffer instead of copying each column out.
pub(crate) fn build_binary_index(graph: &GraphData) -> Vec<u8> {
    let c = build_slim_columns(graph);
    let n = c.n;

    let (id_blob, id_off) = front_code(&c.ids);
    let (name_blob, name_off) = front_code(&c.names);
    let id_hash: Vec<u32> = c.ids.iter().map(|s| fnv1a32(s.as_bytes())).collect();

    // Boundary travels as a flag column rather than the JSON encoding's sparse
    // index list: the client wants `isBoundary(i)` per node, and one byte each
    // is both smaller than a `Set` of indices in the browser and O(1) to read.
    let mut boundary_flags = vec![0u8; n];
    for &i in &c.boundary {
        boundary_flags[i as usize] = 1;
    }

    let meta = serde_json::json!({
        "v": NIDX_VERSION,
        "n": n,
        "nodeCount": n,
        "edgeCount": c.edge_count,
        "block": NIDX_BLOCK,
        "types": c.type_names,
        "files": c.file_names,
        "nodeTypeCounts": c.node_type_counts,
        "edgeTypeCounts": c.edge_type_counts,
        "boundaryCount": c.boundary.len(),
        "stats": graph.stats,
        "languages": c.languages,
        "kbType": c.kb_type,
    })
    .to_string();

    let sections: Vec<(u32, Section)> = vec![
        (
            nidx::TYPE_IDX,
            Section::Bytes(c.types.iter().map(|&t| t as u8).collect()),
        ),
        (nidx::FILE_IDX, i32_section(&c.files)),
        (nidx::START_LINE, u32_section(&c.start)),
        (nidx::END_LINE, u32_section(&c.end)),
        (nidx::DEG, u32_section(&c.deg)),
        (nidx::BOUNDARY, Section::Bytes(boundary_flags)),
        (nidx::CATALOG_ROOTS, u32_section(&c.catalog_roots)),
        (nidx::ID_BLOB, Section::Bytes(id_blob)),
        (nidx::ID_OFF, u32_section(&id_off)),
        (nidx::NAME_BLOB, Section::Bytes(name_blob)),
        (nidx::NAME_OFF, u32_section(&name_off)),
        (nidx::ID_HASH, u32_section(&id_hash)),
        (nidx::META, Section::Bytes(meta.into_bytes())),
    ];

    let header_len = 8 + 4 + 4 + sections.len() * 12;
    let mut out: Vec<u8> = Vec::with_capacity(header_len + n * 32);
    out.extend_from_slice(NIDX_MAGIC);
    out.extend_from_slice(&NIDX_VERSION.to_le_bytes());
    out.extend_from_slice(&(sections.len() as u32).to_le_bytes());
    // Reserve the table; the offsets are only known as the payload is laid out.
    out.resize(header_len, 0);

    for (slot, (kind, section)) in sections.iter().enumerate() {
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
        let Section::Bytes(bytes) = section;
        let offset = out.len() as u32;
        let len = bytes.len() as u32;
        out.extend_from_slice(bytes);
        let at = 16 + slot * 12;
        out[at..at + 4].copy_from_slice(&kind.to_le_bytes());
        out[at + 4..at + 8].copy_from_slice(&offset.to_le_bytes());
        out[at + 8..at + 12].copy_from_slice(&len.to_le_bytes());
    }
    out
}
