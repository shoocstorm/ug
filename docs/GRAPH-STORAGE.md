# Knowledge Graph Storage with LanceDB (Rust)

## Objective
Implement a knowledge graph storage system in Rust using LanceDB for persistence and local embedding generation. Use explicit embeddings.

## Tech Stack (Rust)

· lancedb crate (Rust binding)
· Embedding: 
```Local embedding model settings for testing:
Model: openai/Qwen3-Embedding-0.6B-4bit-DWQ
Base URL: http://localhost:8000/v1
API Key: 1234
```
· serde, tokio, arrow, polars (optional)

## Data Model
refer to: GraphData

### Nodes Table

· id (string, primary key)
· name (string)
· node_type (string, e.g., "File", "Document")
· description (string)
· file (string)
· ....
· last_update_at (datetime)
· node_text (string) – concatenated field for embedding
· vector (fixed-size list of f32, default length 1024)

### Edges Table

· id (string, primary key)
· source (string, node ID)
· target (string, node ID)
· edge_type (string, relationship)
· properties (string JSON, optional extra information)

### Embedding Generation (Explicit)

· Load local model once at startup
· For each node, build node_text = "{type}: {name}. {description}. Related: {list_of_related_names}"
· Batch process all nodes: encode texts → list of 1024-dimension vectors
· Store vectors in LanceDB
· Support incremental updates so that only updated nodes are re-encoded and re-stored
· Support versioning

## LanceDB Setup

· Connect to local directory: LanceDb::connect("./kg_db")
· Create nodes table with schema (including vector column)
· Create edges table without vector column
· Create vector index on nodes.vector (cosine metric)
· Create full-text search index on nodes.description and nodes.name

## Query Functions (Rust async)

1. Semantic Search

· Accept query string
· Generate query embedding (same model)
· LanceDB vector search → top K node records

2. Hybrid Search

· Vector search + pre-filter using SQL WHERE clause (e.g., type = 'Person')
· Optionally combine with FTS scores

3. Graph Traversal

· Step 1: Find start node(s) via ID or semantic search
· Step 2: Query edges table for source = start_node_id
· Step 3: Fetch target nodes by IDs from nodes table
· Return paths up to N hops

## Implementation Checklist

· Add dependencies: lancedb, arrow, candle-core, ort, tokio
· Download & load all-MiniLM-L6-v2.onnx at runtime
· Define Rust structs for Node and Edge (derive Serialize, Deserialize)
· Implement build_node_text(node: &Node, related_names: &[String]) -> String
· Implement generate_embeddings(texts: &[String]) -> Vec<Vec<f32>>
· Implement create_tables(): create LanceDB tables, vector index, FTS index
· Implement insert_nodes() + insert_edges() (batch writes)
· Implement update_node() (recompute embedding on change)
· Implement semantic_search(), hybrid_search(), traverse()
· Add versioning: LanceDB automatically versions on write; use time-travel if needed

## Testing Criteria

· Insert ≥100 nodes, verify vector search returns semantically similar nodes
· Verify FTS returns exact matches for name/description
· Verify two-hop traversal works correctly via edges table
· Confirm vector dimension = 384

## Deliverables

· Rust crate kg_lancedb with public modules: model, embed, db, query
· Example binary demo.rs that:
  1. Creates tables
  2. Inserts sample graph (people, documents, relationships)
  3. Runs semantic search, hybrid search, traversal
  4. Prints results
· Cargo.toml with all dependencies
