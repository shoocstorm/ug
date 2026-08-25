//! End-to-end tests for the Java language indexer and the graph it feeds.
//!
//! Covers the full pipeline: file-walker pickup → tree-sitter parse →
//! symbol / import / annotation extraction → graph construction. Tests
//! stage a real multi-file Java project into a per-test `TempDir`, because
//! everything worth checking here is *cross-file*: whether an import lands
//! on the type it names, whether a call lands on the one method it can mean,
//! whether an interface call reaches its implementation.
//!
//! Unit-level extraction (qualified names, receiver typing, route
//! composition, Javadoc) is covered by the `#[cfg(test)]` module inside the
//! indexer itself; this file is about what survives into the graph.

use std::collections::HashSet;
use std::fs;
use tempfile::TempDir;
use ultragraph::types::{GraphData, GraphEdgeType, GraphNode, GraphNodeType, IndexResult};
use ultragraph::{build_graph, index};

/// Stage a set of `(relative path, contents)` files under one temp dir.
fn stage(files: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    for (rel, content) in files {
        let path = dir.path().join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(&path, content).expect("write fixture");
    }
    dir
}

fn run(files: &[(&str, &str)]) -> (IndexResult, GraphData) {
    let dir = stage(files);
    let json = index(dir.path().to_string_lossy().to_string());
    let result: IndexResult = serde_json::from_str(&json).expect("index() returned invalid JSON");
    let graph: GraphData =
        serde_json::from_str(&build_graph(json)).expect("build_graph returned invalid JSON");
    (result, graph)
}

/// The one node whose display name is `name`, panicking with the full name
/// list when that isn't unique — an ambiguous match here almost always
/// means the qualified-name scheme regressed.
///
/// File nodes are matched by suffix: `index()` names them by their path
/// relative to the canonicalised repo root, and on macOS a `TempDir` under
/// `/var` canonicalises to `/private/var`, so the prefix never strips.
fn node<'a>(graph: &'a GraphData, name: &str) -> &'a GraphNode {
    let matches = |n: &GraphNode| {
        n.name == name
            || (name.ends_with(".java")
                && matches!(n.node_type, GraphNodeType::File)
                && n.name.ends_with(name))
    };
    let hits: Vec<&GraphNode> = graph.nodes.iter().filter(|n| matches(n)).collect();
    assert_eq!(
        hits.len(),
        1,
        "expected exactly one node named {name}, found {}: {:?}",
        hits.len(),
        graph.nodes.iter().map(|n| &n.name).collect::<Vec<_>>()
    );
    hits[0]
}

fn has_edge(graph: &GraphData, source: &str, target: &str, edge_type: GraphEdgeType) -> bool {
    let s = node(graph, source).id.clone();
    let t = node(graph, target).id.clone();
    graph
        .edges
        .iter()
        .any(|e| &*e.source == s.as_str() && &*e.target == t.as_str() && e.edge_type == edge_type)
}

/// Display names of everything `source` points at over `edge_type`.
fn targets(graph: &GraphData, source: &str, edge_type: GraphEdgeType) -> HashSet<String> {
    let s = node(graph, source).id.clone();
    let by_id: std::collections::HashMap<&str, &str> = graph
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), n.name.as_str()))
        .collect();
    graph
        .edges
        .iter()
        .filter(|e| &*e.source == s.as_str() && e.edge_type == edge_type)
        .filter_map(|e| by_id.get(&*e.target).map(|n| n.to_string()))
        .collect()
}

// ─── Walker / language registration ─────────────────────────────────

#[test]
fn java_files_are_indexed_as_java() {
    let (result, _) = run(&[(
        "src/main/java/com/example/A.java",
        "package com.example; class A {}",
    )]);
    assert_eq!(result.files.len(), 1);
    assert_eq!(result.files[0].language, "java");
}

#[test]
fn a_package_named_target_is_not_mistaken_for_a_build_directory() {
    // `IGNORED_DIRS` used to be matched as a substring of the whole path, so
    // every file under a `target` *package* disappeared from the index.
    let (result, _) = run(&[(
        "src/main/java/com/example/target/Goal.java",
        "package com.example.target; class Goal {}",
    )]);
    assert_eq!(
        result.files.len(),
        1,
        "a com.example.target package must still be indexed"
    );
}

#[test]
fn gradle_build_output_is_skipped() {
    // `build/` only counts as output because `build.gradle` sits beside it.
    let (result, _) = run(&[
        ("build.gradle", "plugins { id 'java' }\n"),
        (
            "src/main/java/com/example/A.java",
            "package com.example; class A {}",
        ),
        (
            "build/generated/com/example/B.java",
            "package com.example; class B {}",
        ),
    ]);
    let paths: Vec<&str> = result.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths.len(), 1, "expected only the source file, got {paths:?}");
    assert!(paths[0].contains("src/main/java"));
}

#[test]
fn a_build_directory_with_no_build_descriptor_is_still_indexed() {
    // Without a `build.gradle` next to it, `build/` is just a directory —
    // ignoring it unconditionally would hide real source.
    let (result, _) = run(&[(
        "build/com/example/B.java",
        "package com.example; class B {}",
    )]);
    assert_eq!(result.files.len(), 1);
}

// ─── Imports resolve through qualified names ────────────────────────

#[test]
fn an_import_wires_the_two_files_together() {
    // The headline fix. `com.example.repo` is a package, not a path: the
    // filesystem resolver found nothing for it, so a Java project's entire
    // file-to-file import layer was empty.
    let (_, graph) = run(&[
        (
            "src/main/java/com/example/svc/OrderService.java",
            r#"
            package com.example.svc;
            import com.example.repo.OrderRepository;
            class OrderService {
                private OrderRepository repo;
            }
            "#,
        ),
        (
            "src/main/java/com/example/repo/OrderRepository.java",
            "package com.example.repo; public class OrderRepository {}",
        ),
    ]);

    assert!(
        has_edge(
            &graph,
            "src/main/java/com/example/svc/OrderService.java",
            "src/main/java/com/example/repo/OrderRepository.java",
            GraphEdgeType::Imports
        ),
        "expected a file→file Imports edge, edges: {:?}",
        graph.edges
    );
    assert!(has_edge(
        &graph,
        "src/main/java/com/example/svc/OrderService.java",
        "OrderRepository",
        GraphEdgeType::References
    ));
}

#[test]
fn a_wildcard_import_reaches_every_type_in_the_package() {
    let (_, graph) = run(&[
        (
            "src/main/java/com/example/app/App.java",
            r#"
            package com.example.app;
            import com.example.model.*;
            class App {}
            "#,
        ),
        (
            "src/main/java/com/example/model/Order.java",
            "package com.example.model; public class Order {}",
        ),
        (
            "src/main/java/com/example/model/Customer.java",
            "package com.example.model; public class Customer {}",
        ),
    ]);

    let referenced = targets(
        &graph,
        "src/main/java/com/example/app/App.java",
        GraphEdgeType::References,
    );
    assert!(referenced.contains("Order"), "got {referenced:?}");
    assert!(referenced.contains("Customer"), "got {referenced:?}");
}

#[test]
fn an_import_of_a_type_that_is_not_in_the_repo_adds_no_edge() {
    // A JDK or third-party import has to stay out of the graph rather than
    // producing a near-miss basename match.
    let (_, graph) = run(&[(
        "src/main/java/com/example/A.java",
        r#"
        package com.example;
        import java.util.List;
        class A {}
        "#,
    )]);
    let file_id = node(&graph, "src/main/java/com/example/A.java").id.clone();
    let outgoing: Vec<&GraphEdgeType> = graph
        .edges
        .iter()
        .filter(|e| &*e.source == file_id.as_str() && e.edge_type != GraphEdgeType::Contains)
        .map(|e| &e.edge_type)
        .collect();
    assert!(outgoing.is_empty(), "unexpected edges: {outgoing:?}");
}

// ─── Structure ──────────────────────────────────────────────────────

#[test]
fn methods_hang_off_the_class_that_declares_them() {
    let (_, graph) = run(&[(
        "src/main/java/com/example/A.java",
        r#"
        package com.example;
        class A {
            private String id;
            void go() {}
        }
        "#,
    )]);

    assert!(has_edge(&graph, "A", "A.go", GraphEdgeType::Contains));
    assert!(has_edge(&graph, "A", "A.id", GraphEdgeType::Contains));
    // The file edge is kept as well, so consumers that walk file → symbol
    // keep working.
    assert!(has_edge(
        &graph,
        "src/main/java/com/example/A.java",
        "A.go",
        GraphEdgeType::Contains
    ));
}

#[test]
fn same_named_methods_in_different_classes_stay_distinct() {
    let (_, graph) = run(&[
        (
            "src/main/java/com/example/A.java",
            "package com.example; class A { void execute() {} }",
        ),
        (
            "src/main/java/com/example/B.java",
            "package com.example; class B { void execute() {} }",
        ),
    ]);
    // Both exist under distinct display names, so neither `node()` call is
    // ambiguous — the whole point of qualifying members by their type.
    assert_eq!(node(&graph, "A.execute").name, "A.execute");
    assert_eq!(node(&graph, "B.execute").name, "B.execute");
}

// ─── Calls land on the right method ─────────────────────────────────

#[test]
fn a_call_lands_on_the_method_of_the_receivers_type() {
    let (_, graph) = run(&[
        (
            "src/main/java/com/example/svc/OrderService.java",
            r#"
            package com.example.svc;
            import com.example.repo.OrderRepository;
            import com.example.audit.AuditLog;
            class OrderService {
                private OrderRepository repo;
                private AuditLog audit;
                void cancel(String id) {
                    repo.save(id);
                }
            }
            "#,
        ),
        (
            "src/main/java/com/example/repo/OrderRepository.java",
            "package com.example.repo; public class OrderRepository { public void save(String id) {} }",
        ),
        (
            "src/main/java/com/example/audit/AuditLog.java",
            "package com.example.audit; public class AuditLog { public void save(String id) {} }",
        ),
    ]);

    let called = targets(&graph, "OrderService.cancel", GraphEdgeType::Calls);
    assert!(
        called.contains("OrderRepository.save"),
        "expected the repository's save, got {called:?}"
    );
    assert!(
        !called.contains("AuditLog.save"),
        "the audit log's save is a different method and must not be linked: {called:?}"
    );
}

#[test]
fn a_call_through_an_interface_reaches_the_implementation() {
    // The dependency-injection case: the service only ever names the
    // interface, so without the fan-out the graph has no path from the
    // caller to any code that runs.
    let (_, graph) = run(&[
        (
            "src/main/java/com/example/svc/OrderService.java",
            r#"
            package com.example.svc;
            import com.example.repo.OrderRepository;
            class OrderService {
                private OrderRepository repo;
                void cancel(String id) { repo.save(id); }
            }
            "#,
        ),
        (
            "src/main/java/com/example/repo/OrderRepository.java",
            "package com.example.repo; public interface OrderRepository { void save(String id); }",
        ),
        (
            "src/main/java/com/example/repo/JdbcOrderRepository.java",
            r#"
            package com.example.repo;
            public class JdbcOrderRepository implements OrderRepository {
                public void save(String id) {}
            }
            "#,
        ),
    ]);

    let called = targets(&graph, "OrderService.cancel", GraphEdgeType::Calls);
    assert!(
        called.contains("OrderRepository.save"),
        "the declared target should still be linked: {called:?}"
    );
    assert!(
        called.contains("JdbcOrderRepository.save"),
        "the implementation should be reachable: {called:?}"
    );
}

#[test]
fn an_overriding_method_points_at_what_it_overrides() {
    let (_, graph) = run(&[
        (
            "src/main/java/com/example/repo/OrderRepository.java",
            "package com.example.repo; public interface OrderRepository { void save(String id); }",
        ),
        (
            "src/main/java/com/example/repo/JdbcOrderRepository.java",
            r#"
            package com.example.repo;
            public class JdbcOrderRepository implements OrderRepository {
                @Override
                public void save(String id) {}
            }
            "#,
        ),
    ]);

    assert!(
        has_edge(
            &graph,
            "JdbcOrderRepository.save",
            "OrderRepository.save",
            GraphEdgeType::Overrides
        ),
        "edges: {:?}",
        graph.edges
    );
}

#[test]
fn an_inherited_method_resolves_up_the_superclass_chain() {
    let (_, graph) = run(&[
        (
            "src/main/java/com/example/base/Base.java",
            "package com.example.base; public class Base { public void log(String m) {} }",
        ),
        (
            "src/main/java/com/example/svc/Child.java",
            r#"
            package com.example.svc;
            import com.example.base.Base;
            public class Child extends Base {
                void go() { log("hi"); }
            }
            "#,
        ),
    ]);

    let called = targets(&graph, "Child.go", GraphEdgeType::Calls);
    assert!(called.contains("Base.log"), "got {called:?}");
}

/// `new Order(..)` still resolves to the declared constructor, but it is an
/// `Instantiates` edge rather than a `Calls` one: constructing a value and
/// invoking a method are different relationships, and conflating them left
/// "who calls this class" with no meaningful answer.
#[test]
fn a_constructor_call_lands_on_the_constructor() {
    let (_, graph) = run(&[
        (
            "src/main/java/com/example/model/Order.java",
            "package com.example.model; public class Order { public Order(String id) {} }",
        ),
        (
            "src/main/java/com/example/svc/Maker.java",
            r#"
            package com.example.svc;
            import com.example.model.Order;
            class Maker { Order make() { return new Order("x"); } }
            "#,
        ),
    ]);

    let built = targets(&graph, "Maker.make", GraphEdgeType::Instantiates);
    assert!(built.contains("Order.Order"), "got {built:?}");

    let called = targets(&graph, "Maker.make", GraphEdgeType::Calls);
    assert!(
        !called.contains("Order.Order"),
        "construction must not also be reported as a call: {called:?}"
    );
}

#[test]
fn a_supertype_in_another_package_is_linked() {
    let (_, graph) = run(&[
        (
            "src/main/java/com/example/base/AbstractRepo.java",
            "package com.example.base; public abstract class AbstractRepo<T> {}",
        ),
        (
            "src/main/java/com/example/repo/OrderRepo.java",
            r#"
            package com.example.repo;
            import com.example.base.AbstractRepo;
            import com.example.model.Order;
            public class OrderRepo extends AbstractRepo<Order> {}
            "#,
        ),
    ]);

    // Generic arguments used to travel with the name, so `AbstractRepo<Order>`
    // matched no declared type and the edge was dropped.
    assert!(has_edge(
        &graph,
        "OrderRepo",
        "AbstractRepo",
        GraphEdgeType::Extends
    ));
}

// ─── Annotations and routes ─────────────────────────────────────────

#[test]
fn a_handler_gets_a_route_node_carrying_its_url() {
    let (_, graph) = run(&[(
        "src/main/java/com/example/web/OrderController.java",
        r#"
        package com.example.web;
        @RestController
        @RequestMapping("/api/orders")
        class OrderController {
            @GetMapping("/{id}")
            Order find(String id) { return null; }
        }
        "#,
    )]);

    let route = node(&graph, "GET /api/orders/{id}");
    assert_eq!(route.node_type, GraphNodeType::Route);
    assert!(has_edge(
        &graph,
        "GET /api/orders/{id}",
        "OrderController.find",
        GraphEdgeType::References
    ));
    assert_eq!(
        node(&graph, "OrderController.find").route.as_deref(),
        Some("GET /api/orders/{id}")
    );
}

#[test]
fn annotations_reach_the_graph_node() {
    let (_, graph) = run(&[(
        "src/main/java/com/example/model/Order.java",
        r#"
        package com.example.model;
        @Entity
        @Table(name = "orders")
        class Order {}
        "#,
    )]);

    let names: Vec<&str> = node(&graph, "Order")
        .annotations
        .iter()
        .map(|a| a.name.as_str())
        .collect();
    assert_eq!(names, vec!["Entity", "Table"]);
}

// ─── Classification ─────────────────────────────────────────────────

#[test]
fn a_java_test_is_classified_as_a_test_not_as_the_layer_it_lives_in() {
    // `src/test/java/.../service/OrderServiceTest.java` used to come out as
    // a `Service`, because the only rule that saw it was the `/service/`
    // directory heuristic.
    let (result, _) = run(&[(
        "src/test/java/com/example/service/OrderServiceTest.java",
        r#"
        package com.example.service;
        class OrderServiceTest {
            @Test
            void cancels() {}
        }
        "#,
    )]);
    assert_eq!(
        result.files[0].classification,
        Some(ultragraph::types::FileClassification::Test)
    );
}

#[test]
fn a_test_is_recognised_from_its_annotation_alone() {
    let (result, _) = run(&[(
        "src/main/java/com/example/Checks.java",
        r#"
        package com.example;
        class Checks {
            @Test
            void works() {}
        }
        "#,
    )]);
    assert_eq!(
        result.files[0].classification,
        Some(ultragraph::types::FileClassification::Test)
    );
}
