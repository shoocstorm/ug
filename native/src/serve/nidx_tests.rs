//! Tests for the binary node index (`/api/graph/nodes.bin`) — see the
//! `Binary slim index` section of `serve.rs` and §Round 2 of
//! `docs/dev/PERF-TUNING-JOURNEY.md`.
//!
//! The frame is read by hand-written JS in `native/src/vis/js/`, which no Rust
//! test can execute. So what is pinned here is everything the client *assumes*:
//! that the two encodings describe the same graph, that the front coding round
//! trips, that every section is 4-byte aligned (the client takes typed-array
//! views straight over the buffer), and that the id hash is the exact function
//! the client recomputes.

use super::{build_binary_index, build_slim_index, fnv1a32, front_code, NIDX_BLOCK, NIDX_MAGIC};
use ultragraph::types::{GraphData, GraphEdge, GraphEdgeType, GraphNode, GraphNodeType};

fn node(id: &str, name: &str, ty: GraphNodeType, file: Option<&str>) -> GraphNode {
    GraphNode {
        id: id.into(),
        name: name.into(),
        node_type: ty,
        file: file.map(|f| f.into()),
        start_line: Some(1),
        end_line: Some(9),
        ..Default::default()
    }
}

fn edge(source: &str, target: &str, edge_type: GraphEdgeType) -> GraphEdge {
    GraphEdge {
        source: source.into(),
        target: target.into(),
        edge_type,
    }
}

/// A graph with enough shape to exercise every column: two files under a
/// folder, symbols inside them, a node with no file, and more than
/// `NIDX_BLOCK` nodes so the front coding actually restarts a block.
fn fixture() -> GraphData {
    let mut nodes = vec![
        node("src", "src", GraphNodeType::Folder, None),
        node("src/a.rs", "a.rs", GraphNodeType::File, Some("src/a.rs")),
        node("src/b.rs", "b.rs", GraphNodeType::File, Some("src/b.rs")),
    ];
    for i in 0..40 {
        nodes.push(node(
            &format!("src/a.rs::module::function_number_{i:03}"),
            &format!("function_number_{i:03}"),
            GraphNodeType::Function,
            Some("src/a.rs"),
        ));
    }
    let edges = vec![
        edge("src", "src/a.rs", GraphEdgeType::Contains),
        edge("src", "src/b.rs", GraphEdgeType::Contains),
        edge(
            "src/a.rs",
            "src/a.rs::module::function_number_000",
            GraphEdgeType::Contains,
        ),
        edge(
            "src/a.rs::module::function_number_000",
            "src/a.rs::module::function_number_001",
            GraphEdgeType::Calls,
        ),
    ];
    GraphData {
        nodes,
        edges,
        stats: None,
        resolution: None,
    }
}

/// Decode the frame the way the client does: header, section table, then a
/// slice per kind.
fn sections(frame: &[u8]) -> std::collections::HashMap<u32, (usize, usize)> {
    assert_eq!(&frame[..8], NIDX_MAGIC, "magic");
    let version = u32::from_le_bytes(frame[8..12].try_into().unwrap());
    assert_eq!(version, 1);
    let count = u32::from_le_bytes(frame[12..16].try_into().unwrap()) as usize;
    let mut out = std::collections::HashMap::new();
    for slot in 0..count {
        let at = 16 + slot * 12;
        let kind = u32::from_le_bytes(frame[at..at + 4].try_into().unwrap());
        let off = u32::from_le_bytes(frame[at + 4..at + 8].try_into().unwrap()) as usize;
        let len = u32::from_le_bytes(frame[at + 8..at + 12].try_into().unwrap()) as usize;
        assert!(off + len <= frame.len(), "section {kind} runs past the frame");
        out.insert(kind, (off, len));
    }
    assert_eq!(out.len(), count, "duplicate section kind");
    out
}

fn u32s(frame: &[u8], (off, len): (usize, usize)) -> Vec<u32> {
    frame[off..off + len]
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect()
}

/// Reconstruct the front-coded column, which is the part of this format the
/// client has to get exactly right.
fn decode_front(blob: &[u8], off: &[u32]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(off.len() - 1);
    let mut prev: Vec<u8> = Vec::new();
    for i in 0..off.len() - 1 {
        let rec = &blob[off[i] as usize..off[i + 1] as usize];
        let shared = rec[0] as usize;
        let mut cur = prev[..shared].to_vec();
        cur.extend_from_slice(&rec[1..]);
        out.push(String::from_utf8(cur.clone()).expect("utf8"));
        prev = cur;
    }
    out
}

#[test]
fn front_coding_round_trips() {
    let values: Vec<String> = (0..100)
        .map(|i| format!("src/some/deep/path/file.rs::module::symbol_{i:04}"))
        .collect();
    let refs: Vec<&str> = values.iter().map(|s| s.as_str()).collect();
    let (blob, off) = front_code(&refs);
    assert_eq!(off.len(), refs.len() + 1);
    assert_eq!(decode_front(&blob, &off), values);

    // The whole point: it is meaningfully smaller than the raw bytes.
    let raw: usize = refs.iter().map(|s| s.len()).sum();
    assert!(
        blob.len() * 2 < raw,
        "front coding gained nothing: {} vs {raw}",
        blob.len()
    );
}

#[test]
fn front_coding_restarts_every_block_so_lookup_is_bounded() {
    let values: Vec<String> = (0..50).map(|i| format!("aaaaaaaaaaaaaaaa{i:04}")).collect();
    let refs: Vec<&str> = values.iter().map(|s| s.as_str()).collect();
    let (blob, off) = front_code(&refs);
    for i in 0..refs.len() {
        let shared = blob[off[i] as usize];
        if i % NIDX_BLOCK == 0 {
            assert_eq!(shared, 0, "entry {i} starts a block and must be whole");
        }
    }
}

#[test]
fn front_coding_handles_the_degenerate_inputs() {
    // Empty column, empty strings, one entry, and a pair where the second is a
    // prefix of the first (shared must not run past the shorter string).
    for values in [
        vec![],
        vec![""],
        vec!["", "", ""],
        vec!["abcdef", "abc"],
        vec!["abc", "abcdef"],
    ] {
        let (blob, off) = front_code(&values);
        let want: Vec<String> = values.iter().map(|s| s.to_string()).collect();
        assert_eq!(decode_front(&blob, &off), want, "round trip for {values:?}");
    }
}

#[test]
fn every_section_is_four_byte_aligned() {
    // The client takes `Uint32Array`/`Int32Array` views directly over the
    // response buffer. An unaligned offset throws a RangeError there, which is
    // a blank page rather than a slow one.
    let frame = build_binary_index(&fixture());
    for (kind, (off, _)) in sections(&frame) {
        assert_eq!(off % 4, 0, "section {kind} at unaligned offset {off}");
    }
}

#[test]
fn slim_index_encodings_describe_the_same_graph() {
    let graph = fixture();
    let json: serde_json::Value = serde_json::from_str(&build_slim_index(&graph)).unwrap();
    let frame = build_binary_index(&graph);
    let secs = sections(&frame);

    let meta: serde_json::Value =
        serde_json::from_slice(&frame[secs[&13].0..secs[&13].0 + secs[&13].1]).unwrap();

    assert_eq!(meta["n"], json["n"]);
    assert_eq!(meta["edgeCount"], json["edgeCount"]);
    assert_eq!(meta["types"], json["types"]);
    assert_eq!(meta["files"], json["files"]);
    assert_eq!(meta["nodeTypeCounts"], json["nodeTypeCounts"]);
    assert_eq!(meta["edgeTypeCounts"], json["edgeTypeCounts"]);
    assert_eq!(meta["stats"], json["stats"]);

    let n = meta["n"].as_u64().unwrap() as usize;

    // Strings.
    let ids = decode_front(
        &frame[secs[&8].0..secs[&8].0 + secs[&8].1],
        &u32s(&frame, secs[&9]),
    );
    let names = decode_front(
        &frame[secs[&10].0..secs[&10].0 + secs[&10].1],
        &u32s(&frame, secs[&11]),
    );
    let json_ids: Vec<String> = serde_json::from_value(json["ids"].clone()).unwrap();
    let json_names: Vec<String> = serde_json::from_value(json["names"].clone()).unwrap();
    assert_eq!(ids, json_ids, "id column");
    assert_eq!(names, json_names, "name column");

    // Numeric columns.
    assert_eq!(
        u32s(&frame, secs[&3]),
        serde_json::from_value::<Vec<u32>>(json["startLine"].clone()).unwrap()
    );
    assert_eq!(
        u32s(&frame, secs[&4]),
        serde_json::from_value::<Vec<u32>>(json["endLine"].clone()).unwrap()
    );
    assert_eq!(
        u32s(&frame, secs[&5]),
        serde_json::from_value::<Vec<u32>>(json["deg"].clone()).unwrap()
    );
    assert_eq!(
        u32s(&frame, secs[&7]),
        serde_json::from_value::<Vec<u32>>(json["catalogRoots"].clone()).unwrap()
    );

    let type_idx = &frame[secs[&1].0..secs[&1].0 + secs[&1].1];
    let json_types: Vec<u32> = serde_json::from_value(json["typeIdx"].clone()).unwrap();
    assert_eq!(type_idx.len(), n);
    for (i, &t) in type_idx.iter().enumerate() {
        assert_eq!(t as u32, json_types[i], "typeIdx[{i}]");
    }

    let file_bytes = &frame[secs[&2].0..secs[&2].0 + secs[&2].1];
    let file_idx: Vec<i32> = file_bytes
        .chunks_exact(4)
        .map(|c| i32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    let json_files: Vec<i64> = serde_json::from_value(json["fileIdx"].clone()).unwrap();
    assert_eq!(file_idx.len(), n);
    for (i, &f) in file_idx.iter().enumerate() {
        assert_eq!(f as i64, json_files[i], "fileIdx[{i}]");
    }

    // Boundary: sparse index list in JSON, flag column in the frame.
    let flags = &frame[secs[&6].0..secs[&6].0 + secs[&6].1];
    let json_boundary: Vec<u32> = serde_json::from_value(json["boundary"].clone()).unwrap();
    assert_eq!(flags.len(), n);
    for i in 0..n {
        let want = json_boundary.contains(&(i as u32));
        assert_eq!(flags[i] == 1, want, "boundary[{i}]");
    }
    assert_eq!(
        meta["boundaryCount"].as_u64().unwrap() as usize,
        json_boundary.len()
    );
}

#[test]
fn the_id_hash_column_matches_the_ids() {
    let graph = fixture();
    let frame = build_binary_index(&graph);
    let secs = sections(&frame);
    let hashes = u32s(&frame, secs[&12]);
    let ids = decode_front(
        &frame[secs[&8].0..secs[&8].0 + secs[&8].1],
        &u32s(&frame, secs[&9]),
    );
    assert_eq!(hashes.len(), ids.len());
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(hashes[i], fnv1a32(id.as_bytes()), "hash of {id}");
    }
}

#[test]
fn nidx_hash_matches_the_client() {
    // These are the values `fnv1a32` in `00-preamble.js` must produce for the
    // same input, and they are what makes the client's `id → index` table find
    // anything at all. Computed from the FNV-1a 32-bit reference definition:
    // offset basis 2166136261, prime 16777619, one byte at a time.
    assert_eq!(fnv1a32(b""), 2166136261);
    assert_eq!(fnv1a32(b"a"), 0xe40c292c);
    assert_eq!(fnv1a32(b"foobar"), 0xbf9cf968);
    // Non-ASCII must hash by UTF-8 *bytes*, not by code unit — the client
    // encodes with TextEncoder for exactly this reason.
    assert_eq!(fnv1a32("é".as_bytes()), fnv1a32(&[0xc3, 0xa9]));
}

// ---------- Cross-language round trip ----------
//
// The bug this exists for: the client's front-coded decoder grew its scratch
// buffer without copying what was already in it, so any id that crossed a
// growth boundary came back with its head cut off. Nothing threw — the id just
// hashed to something else and the node became unfindable. Every Rust-side
// test above passed throughout.
//
// So the decoder is executed, on a frame this encoder produced, against the
// ids it was built from.

/// Pull the store + frame decoder out of the assembled client source.
///
/// By marker rather than by line number, and it panics loudly if the markers
/// move — a silently empty slice would make this test pass forever.
fn client_decoder_source() -> String {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/vis/js/02-dialogs.js"),
    )
    .expect("read 02-dialogs.js");
    let start = src
        .find("        const EMPTY_LIST = Object.freeze([]);")
        .expect("EMPTY_LIST marker moved — update client_decoder_source");
    let tail = src
        .find("        function transformSlimBinary(buffer) {")
        .expect("transformSlimBinary marker moved — update client_decoder_source");
    let end = tail
        + src[tail..]
            .find("\n        }\n")
            .expect("transformSlimBinary has no close")
        + "\n        }\n".len();
    src[start..end].to_string()
}

#[test]
fn the_client_decodes_every_id_this_encoder_writes() {
    let Ok(node) = which_node() else {
        eprintln!("skipping: `node` is not on PATH");
        return;
    };

    // The fixture is the test. A decoder that reconstructs each entry from
    // scratch passes on almost any input; what breaks one is an entry that
    // makes the reconstruction buffer *grow* while a shared prefix is already
    // sitting in it.
    //
    // So: a long shared prefix, every id in the first blocks comfortably under
    // a plausible initial buffer size, and then — mid-block, so `shared` is
    // large — an id far past it. Get that wrong and the id comes back with its
    // head missing, which is not an error, just a node that cannot be found.
    //
    // Then the awkward cases: non-ASCII (the hash is over UTF-8 bytes, not code
    // units), an id that is a strict prefix of the one before it, and a
    // one-character id.
    let prefix = "p".repeat(240);
    let mut nodes: Vec<GraphNode> = Vec::new();
    for i in 0..16 {
        nodes.push(node_named(&format!("{prefix}{i:02}")));
    }
    // Index 16 starts a block and is stored whole; 17 onwards share ~240 bytes
    // with it and then run long.
    nodes.push(node_named(&format!("{prefix}aaa")));
    for i in 0..8 {
        nodes.push(node_named(&format!("{prefix}aaa{}{i}", "z".repeat(400))));
    }
    let deep = "a".repeat(300);
    for i in 0..40 {
        nodes.push(node_named(&format!("{deep}/module/very_long_symbol_name_{i:04}")));
    }
    nodes.push(node_named(&format!("{deep}/module")));
    nodes.push(node_named("ünïcödé::symbol::ß"));
    nodes.push(node_named("ünïcödé::symbol::ßß"));
    nodes.push(node_named("x"));
    let graph = GraphData {
        nodes,
        edges: vec![],
        stats: None,
        resolution: None,
    };

    let frame = build_binary_index(&graph);
    let ids: Vec<&str> = graph.nodes.iter().map(|n| n.id.as_str()).collect();

    let dir = std::env::temp_dir().join(format!("ug-nidx-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let frame_path = dir.join("frame.bin");
    let ids_path = dir.join("ids.json");
    let driver_path = dir.join("driver.mjs");
    std::fs::write(&frame_path, &frame).unwrap();
    std::fs::write(&ids_path, serde_json::to_string(&ids).unwrap()).unwrap();

    let driver = format!(
        r#"
import {{ readFileSync }} from 'node:fs';
// Stubs for the two globals the slice touches while installing an index.
const state = {{}};
const window = {{ innerWidth: 1600, innerHeight: 1200 }};
{decoder}
const size = readFileSync({frame:?});
const ab = new ArrayBuffer(size.length);
new Uint8Array(ab).set(size);
installNodeIndex(decodeNodeIndexFrame(ab));
const want = JSON.parse(readFileSync({ids:?}, 'utf8'));
const store = state.nodeStore;
if (store.nodeCount !== want.length) throw new Error('node count ' + store.nodeCount + ' != ' + want.length);
for (let i = 0; i < want.length; i++) {{
  const got = store.idAt(i);
  if (got !== want[i]) throw new Error('idAt(' + i + ') decoded ' + JSON.stringify(got) + ' want ' + JSON.stringify(want[i]));
  if (store.indexOf(want[i]) !== i) throw new Error('indexOf(' + JSON.stringify(want[i]) + ') != ' + i);
  if (!state.nodeById.has(want[i])) throw new Error('has() false for ' + JSON.stringify(want[i]));
  if (state.nodeById.get(want[i]).id !== want[i]) throw new Error('get() wrong node for ' + JSON.stringify(want[i]));
}}
if (store.indexOf('no such node anywhere') !== -1) throw new Error('a miss resolved to a node');
console.log('ok ' + want.length);
"#,
        decoder = client_decoder_source(),
        frame = frame_path,
        ids = ids_path,
    );
    std::fs::write(&driver_path, driver).unwrap();

    let out = std::process::Command::new(node)
        .arg(&driver_path)
        .output()
        .expect("run node");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "client decoder rejected this encoder's frame:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("ok {}", ids.len())
    );
}

fn node_named(id: &str) -> GraphNode {
    node(id, "", GraphNodeType::Function, None)
}

/// `node` on PATH, or an error to skip on. CI images without it should not
/// fail this suite — the Rust-side format tests above still run.
fn which_node() -> Result<std::path::PathBuf, ()> {
    let path = std::env::var_os("PATH").ok_or(())?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("node");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(())
}

// ---------- /api/graph/search ranking ----------
//
// In server mode this endpoint *is* the page's search box, so where the
// `limit` cut falls decides what the user sees. It used to fall on whichever
// matches came first in the node list.

#[test]
fn search_ranking_puts_the_best_matches_inside_the_limit() {
    use super::rank_search_matches;

    // `zzz` matches all four. The two that match early in the name must win the
    // cut over the two that match late, whatever order they are stored in.
    let names = [
        "unrelated_long_name_with_zzz_at_the_end",
        "zzz_short",
        "another_name_ending_in_zzz",
        "zzz_a_bit_longer",
    ];
    let ranked = rank_search_matches(&names, "zzz", 2);
    assert_eq!(ranked, vec![1, 3], "expected the two prefix matches, best first");

    // The full ordering, unlimited: prefix matches first (shorter name wins the
    // tie), then the late matches by position.
    let all = rank_search_matches(&names, "zzz", 10);
    assert_eq!(all, vec![1, 3, 2, 0]);

    // Degenerate inputs must not panic — an empty match set used to reach
    // `select_nth_unstable(0)` on an empty slice.
    assert!(rank_search_matches(&names, "no-such-substring", 10).is_empty());
    assert!(rank_search_matches(&[], "zzz", 10).is_empty());
    assert!(rank_search_matches(&names, "zzz", 0).is_empty());
}
