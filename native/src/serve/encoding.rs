//! `encoding.rs` — split out of `serve.rs`; see `docs/dev/REFACTOR-TRACKING.md`.

use std::sync::OnceLock;

use axum::body::{Body, Bytes};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;

// ---------- Encoded asset (identity + gzip + br, all pre-built) ----------

/// `Clone` is cheap and refcounted: `Bytes` clones bump a refcount rather than
/// copying, which is what lets a cached asset be handed out of a `OnceLock`
/// without holding a borrow across an await.
#[derive(Clone)]
/// One response body, plus whichever compressed forms have been asked for.
///
/// Both encodings are built on first request rather than up front. Eagerly
/// compressing at construction meant every `graph.json` was gzip-9'd *and*
/// brotli-9'd before the server would answer anything — minutes of startup CPU
/// on a 330 MB index, holding four copies of it, to produce two bodies of which
/// any one client uses at most one. Past the server-mode cutoff
/// (`graph.server_mode_bytes`, default `GRAPH_SERVER_MODE_BYTES`) the browser
/// is told to use the slim index and never fetches `graph.json` at all, so on
/// exactly the graphs where that cost hurt most, all of it was waste.
///
/// The trade is that the first client wanting an encoding waits for it on a
/// runtime worker instead of the process waiting at startup. That is the better
/// end of the deal — it is paid once, only when something actually wants the
/// bytes, and the large-graph case skips it — but see the follow-up note in
/// `docs/dev/PERF-TUNING-JOURNEY.md` about warming it in the background.
pub(crate) struct EncodedAsset {
    pub(crate) identity: Bytes,
    pub(crate) gzip: OnceLock<Bytes>,
    pub(crate) brotli: OnceLock<Bytes>,
    pub(crate) content_type: HeaderValue,
}

impl EncodedAsset {
    /// Wrap bytes. **No compression happens here** — see the field comments.
    pub(crate) fn new(raw: Vec<u8>, content_type: &'static str) -> Self {
        Self {
            identity: Bytes::from(raw),
            gzip: OnceLock::new(),
            brotli: OnceLock::new(),
            content_type: HeaderValue::from_static(content_type),
        }
    }

    fn gzip(&self) -> &Bytes {
        self.gzip.get_or_init(|| compress_gzip(&self.identity))
    }

    fn brotli(&self) -> &Bytes {
        self.brotli.get_or_init(|| compress_brotli(&self.identity))
    }

    /// Bytes this asset is holding *right now*, for the snapshot cache
    /// budget. An encoding nobody has asked for costs nothing and is not
    /// counted — which makes the budget an account of real memory rather
    /// than of memory we used to allocate unconditionally.
    pub(crate) fn retained(&self) -> usize {
        self.identity.len()
            + self.gzip.get().map_or(0, |b| b.len())
            + self.brotli.get().map_or(0, |b| b.len())
    }
}

fn compress_gzip(data: &[u8]) -> Bytes {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;
    let mut enc = GzEncoder::new(Vec::with_capacity(data.len() / 4), Compression::new(9));
    enc.write_all(data).expect("gzip encode");
    Bytes::from(enc.finish().expect("gzip finish"))
}

fn compress_brotli(data: &[u8]) -> Bytes {
    use brotli::enc::BrotliEncoderParams;
    let mut out = Vec::with_capacity(data.len() / 4);
    let mut params = BrotliEncoderParams::default();
    // Quality 9 is a good size/CPU tradeoff for startup-time compression
    // (11 is slightly smaller but several times slower).
    params.quality = 9;
    params.lgwin = 22;
    let mut input = data;
    brotli::BrotliCompress(&mut input, &mut out, &params).expect("brotli compress");
    Bytes::from(out)
}

// ---------- Encoding negotiation ----------

#[derive(Copy, Clone, PartialEq, Eq)]
pub(crate) enum Encoding {
    Identity,
    Gzip,
    Brotli,
}

pub(crate) fn pick_encoding(headers: &HeaderMap) -> Encoding {
    let Some(accept) = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
    else {
        return Encoding::Identity;
    };
    let mut has_gzip = false;
    let mut has_br = false;
    for part in accept.split(',') {
        let token = part
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        match token.as_str() {
            "br" => has_br = true,
            "gzip" => has_gzip = true,
            _ => {}
        }
    }
    if has_br {
        Encoding::Brotli
    } else if has_gzip {
        Encoding::Gzip
    } else {
        Encoding::Identity
    }
}

/// Header carrying the body's size *before* `Content-Encoding` was applied.
///
/// `Content-Length` is the compressed size, and a client streaming the body
/// through `response.body.getReader()` counts **decoded** bytes — the browser
/// having already inflated them. Dividing one by the other makes a progress
/// bar that reaches 100% at roughly a tenth of the download and sits there.
/// There is no standard header for this, so the page reads ours.
const UNCOMPRESSED_LENGTH: &str = "x-uncompressed-length";

pub(crate) fn asset_response(asset: &EncodedAsset, headers: &HeaderMap) -> Response {
    // Only the encoding this client asked for is ever built; the other stays
    // uncomputed for the life of the process if nothing requests it.
    let (bytes, encoding) = match pick_encoding(headers) {
        Encoding::Brotli => (asset.brotli().clone(), Some("br")),
        Encoding::Gzip => (asset.gzip().clone(), Some("gzip")),
        Encoding::Identity => (asset.identity.clone(), None),
    };
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, asset.content_type.clone())
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::VARY, "accept-encoding")
        .header(header::CONTENT_LENGTH, bytes.len())
        .header(UNCOMPRESSED_LENGTH, asset.identity.len());
    if let Some(e) = encoding {
        builder = builder.header(header::CONTENT_ENCODING, e);
    }
    builder.body(Body::from(bytes)).expect("build response")
}
