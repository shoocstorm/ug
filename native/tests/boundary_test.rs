//! End-to-end tests for system-boundary tagging.
//!
//! These go through the real pipeline — file walker, tree-sitter parse,
//! per-language extraction, the `indexer::boundary` post-pass, graph build —
//! because the whole point of the post-pass is that it reads facts the
//! extractors produce, and a unit test that hand-builds a `Symbol` would
//! prove only that the matcher works on data no indexer ever emits.
//!
//! The rule registry's own invariants (unique ids, well-formed kinds) are
//! checked by the `#[cfg(test)]` module inside `indexer/boundary.rs`.

use std::fs;
use tempfile::TempDir;
use ultragraph::index;
use ultragraph::types::{Boundary, BoundaryDirection, IndexResult, Symbol};

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

fn run(files: &[(&str, &str)]) -> IndexResult {
    let dir = stage(files);
    let json = index(dir.path().to_string_lossy().to_string());
    serde_json::from_str(&json).expect("index() returned invalid JSON")
}

/// Every symbol in the result, across files.
fn symbols(r: &IndexResult) -> Vec<&Symbol> {
    r.files.iter().flat_map(|f| f.symbols.iter()).collect()
}

fn find<'a>(r: &'a IndexResult, name: &str) -> &'a Symbol {
    symbols(r)
        .into_iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| {
            let all: Vec<&str> = symbols(r).iter().map(|s| s.name.as_str()).collect();
            panic!("no symbol named {name:?}; have {all:?}")
        })
}

fn of_kind<'a>(sym: &'a Symbol, kind: &str) -> &'a Boundary {
    sym.boundaries
        .iter()
        .find(|b| b.kind == kind)
        .unwrap_or_else(|| {
            panic!(
                "{} has no {kind} boundary; has {:?}",
                sym.name,
                sym.boundaries.iter().map(|b| &b.kind).collect::<Vec<_>>()
            )
        })
}

// ── Java: inbound ──────────────────────────────────────────────────────

#[test]
fn a_spring_handler_is_an_inbound_http_endpoint_carrying_its_route() {
    let r = run(&[(
        "src/main/java/com/acme/OrderController.java",
        r#"
        package com.acme;
        import org.springframework.web.bind.annotation.*;

        @RestController
        @RequestMapping("/api/orders")
        public class OrderController {
            @GetMapping("/{id}")
            public Order find(@PathVariable Long id) { return null; }
        }
        "#,
    )]);

    let b = of_kind(find(&r, "OrderController.find"), "http.endpoint");
    assert_eq!(b.direction, BoundaryDirection::Inbound);
    assert_eq!(b.protocol, "http");
    // The composed route is the whole reason this tag is worth having: it
    // appears in no identifier, no path and no Javadoc.
    assert_eq!(b.detail.as_deref(), Some("GET /api/orders/{id}"));
    assert_eq!(b.source, "http.route");
}

#[test]
fn a_jms_listener_is_a_boundary_named_by_its_destination() {
    let r = run(&[(
        "src/main/java/com/acme/OrderListener.java",
        r#"
        package com.acme;
        import org.springframework.jms.annotation.JmsListener;

        public class OrderListener {
            @JmsListener(destination = "orders.inbound")
            public void onMessage(String body) {}
        }
        "#,
    )]);

    let b = of_kind(find(&r, "OrderListener.onMessage"), "mq.listener");
    assert_eq!(b.direction, BoundaryDirection::Inbound);
    assert_eq!(b.protocol, "jms");
    assert_eq!(b.detail.as_deref(), Some("orders.inbound"));
}

#[test]
fn kafka_and_rabbit_listeners_keep_their_own_protocols() {
    let r = run(&[(
        "src/main/java/com/acme/Listeners.java",
        r#"
        package com.acme;
        public class Listeners {
            @KafkaListener(topics = "payments")
            public void onPayment(String m) {}

            @RabbitListener(queues = "audit.q")
            public void onAudit(String m) {}
        }
        "#,
    )]);

    let kafka = of_kind(find(&r, "Listeners.onPayment"), "mq.listener");
    assert_eq!(kafka.protocol, "kafka");
    assert_eq!(kafka.detail.as_deref(), Some("payments"));

    let rabbit = of_kind(find(&r, "Listeners.onAudit"), "mq.listener");
    assert_eq!(rabbit.protocol, "amqp");
    assert_eq!(rabbit.detail.as_deref(), Some("audit.q"));
}

#[test]
fn a_scheduled_job_is_an_inbound_boundary() {
    let r = run(&[(
        "src/main/java/com/acme/Sweeper.java",
        r#"
        package com.acme;
        public class Sweeper {
            @Scheduled(cron = "0 0 * * * *")
            public void sweep() {}
        }
        "#,
    )]);

    let b = of_kind(find(&r, "Sweeper.sweep"), "scheduled.job");
    assert_eq!(b.direction, BoundaryDirection::Inbound);
    assert_eq!(b.detail.as_deref(), Some("0 0 * * * *"));
}

#[test]
fn a_main_method_is_a_cli_entry_point() {
    let r = run(&[(
        "src/main/java/com/acme/App.java",
        r#"
        package com.acme;
        public class App {
            public static void main(String[] args) {}
        }
        "#,
    )]);

    let b = of_kind(find(&r, "App.main"), "cli.command");
    assert_eq!(b.direction, BoundaryDirection::Inbound);
    assert_eq!(b.protocol, "cli");
}

#[test]
fn implementing_the_jms_listener_interface_is_enough() {
    let r = run(&[(
        "src/main/java/com/acme/RawListener.java",
        r#"
        package com.acme;
        import javax.jms.MessageListener;
        public class RawListener implements MessageListener {
            public void onMessage(Message m) {}
        }
        "#,
    )]);

    let b = of_kind(find(&r, "RawListener"), "mq.listener");
    assert_eq!(b.source, "jms.MessageListener");
}

// ── Java: outbound ─────────────────────────────────────────────────────

#[test]
fn calling_a_rest_template_is_an_outbound_http_client() {
    let r = run(&[(
        "src/main/java/com/acme/PricingGateway.java",
        r#"
        package com.acme;
        import org.springframework.web.client.RestTemplate;

        public class PricingGateway {
            private RestTemplate restTemplate;
            public Price fetch(String sku) {
                return restTemplate.getForObject("/price/" + sku, Price.class);
            }
        }
        "#,
    )]);

    let b = of_kind(find(&r, "PricingGateway.fetch"), "http.client");
    assert_eq!(b.direction, BoundaryDirection::Outbound);
    assert_eq!(b.protocol, "http");
}

#[test]
fn a_jpa_repository_is_where_the_system_reaches_the_database() {
    let r = run(&[(
        "src/main/java/com/acme/OrderRepository.java",
        r#"
        package com.acme;
        import org.springframework.data.jpa.repository.JpaRepository;
        public interface OrderRepository extends JpaRepository<Order, Long> {
            Order findBySku(String sku);
        }
        "#,
    )]);

    let b = of_kind(find(&r, "OrderRepository"), "db.access");
    assert_eq!(b.direction, BoundaryDirection::Outbound);
    assert_eq!(b.protocol, "jpa");
}

#[test]
fn a_service_that_only_calls_a_repository_is_not_itself_a_db_boundary() {
    // The line this test defends: the repository is where the process
    // actually talks to the database. If every transitive caller were
    // tagged, `boundary_out` would come to mean "touches business logic"
    // and the blast-radius answer would be the whole service layer.
    let r = run(&[
        (
            "src/main/java/com/acme/OrderRepository.java",
            r#"
            package com.acme;
            import org.springframework.data.jpa.repository.JpaRepository;
            public interface OrderRepository extends JpaRepository<Order, Long> {}
            "#,
        ),
        (
            "src/main/java/com/acme/OrderService.java",
            r#"
            package com.acme;
            public class OrderService {
                private OrderRepository repo;
                public void cancel(Long id) { repo.deleteById(id); }
            }
            "#,
        ),
    ]);

    let svc = find(&r, "OrderService.cancel");
    assert!(
        svc.boundaries.iter().all(|b| b.kind != "db.access"),
        "service method should not be a db boundary, got {:?}",
        svc.boundaries
    );
}

// ── Python ─────────────────────────────────────────────────────────────

#[test]
fn a_flask_route_is_an_inbound_http_endpoint() {
    let r = run(&[(
        "app.py",
        "@app.route(\"/users\")\ndef list_users():\n    pass\n",
    )]);

    let b = of_kind(find(&r, "list_users"), "http.endpoint");
    assert_eq!(b.direction, BoundaryDirection::Inbound);
    assert_eq!(b.detail.as_deref(), Some("/users"));
}

#[test]
fn a_route_is_found_whatever_the_app_object_is_called() {
    // The reason the rule is a `*.route` wildcard: a blueprint or an
    // APIRouter is the same framework under a different variable name, and
    // matching the literal `app` would miss most real Flask codebases.
    let r = run(&[(
        "views.py",
        "@bp.route(\"/health\")\ndef health():\n    pass\n\n@api.get(\"/v2/items\")\ndef items():\n    pass\n",
    )]);

    assert_eq!(
        of_kind(find(&r, "health"), "http.endpoint").detail.as_deref(),
        Some("/health")
    );
    assert_eq!(
        of_kind(find(&r, "items"), "http.endpoint").detail.as_deref(),
        Some("/v2/items")
    );
}

#[test]
fn a_click_command_is_a_cli_boundary() {
    let r = run(&[(
        "cli.py",
        "@click.command()\ndef migrate():\n    pass\n",
    )]);

    let b = of_kind(find(&r, "migrate"), "cli.command");
    assert_eq!(b.protocol, "cli");
}

#[test]
fn an_undecorated_python_function_is_not_a_boundary() {
    let r = run(&[("util.py", "def helper(x):\n    return x + 1\n")]);
    assert!(find(&r, "helper").boundaries.is_empty());
}

// ── TypeScript ─────────────────────────────────────────────────────────

#[test]
fn a_nest_controller_method_is_an_inbound_http_endpoint() {
    let r = run(&[(
        "src/orders.controller.ts",
        r#"
        @Controller('orders')
        export class OrdersController {
            @Get(':id')
            find(id: string) {}
        }
        "#,
    )]);

    let b = of_kind(find(&r, "OrdersController.find"), "http.endpoint");
    assert_eq!(b.direction, BoundaryDirection::Inbound);
    assert_eq!(b.detail.as_deref(), Some(":id"));
}

#[test]
fn a_bare_get_decorator_outside_a_controller_is_not_an_endpoint() {
    // `Get` is a plausible name for anything, so the Nest rules require the
    // declaring class to be a `@Controller`. Without that guard this would
    // be a false positive, and a false boundary is worse than a missing one.
    let r = run(&[(
        "src/thing.ts",
        r#"
        @Entity()
        export class Thing {
            @Get()
            value() {}
        }
        "#,
    )]);

    assert!(
        find(&r, "Thing.value").boundaries.is_empty(),
        "got {:?}",
        find(&r, "Thing.value").boundaries
    );
}

// ── Rust ───────────────────────────────────────────────────────────────

#[test]
fn a_rust_main_is_a_cli_entry_point() {
    let r = run(&[("src/main.rs", "fn main() {\n    println!(\"hi\");\n}\n")]);

    let b = of_kind(find(&r, "main"), "cli.command");
    assert_eq!(b.direction, BoundaryDirection::Inbound);
}

#[test]
fn an_actix_attribute_is_an_inbound_http_endpoint() {
    let r = run(&[(
        "src/routes.rs",
        "#[get(\"/health\")]\nasync fn health() -> String {\n    String::new()\n}\n",
    )]);

    let b = of_kind(find(&r, "health"), "http.endpoint");
    assert_eq!(b.detail.as_deref(), Some("/health"));
}

#[test]
fn rust_attributes_survive_an_interleaved_doc_comment() {
    // Attributes and `///` lines mix freely above an item; neither ends the
    // run, and a doc comment between them must not hide the attribute.
    let r = run(&[(
        "src/routes.rs",
        "#[get(\"/x\")]\n/// Handler docs.\nasync fn handler() {}\n",
    )]);

    let sym = find(&r, "handler");
    assert!(
        !sym.boundaries.is_empty(),
        "doc comment hid the attribute: {:?}",
        sym.annotations
    );
}

#[test]
fn an_ordinary_rust_function_is_not_a_boundary() {
    let r = run(&[("src/lib.rs", "pub fn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n")]);
    assert!(find(&r, "add").boundaries.is_empty());
}

// ── Cross-cutting ──────────────────────────────────────────────────────

#[test]
fn a_handler_that_also_calls_a_client_carries_both_directions() {
    let r = run(&[(
        "src/main/java/com/acme/QuoteController.java",
        r#"
        package com.acme;
        import org.springframework.web.bind.annotation.*;
        import org.springframework.web.client.RestTemplate;

        @RestController
        public class QuoteController {
            private RestTemplate restTemplate;

            @PostMapping("/quote")
            public Quote quote(@RequestBody Ask ask) {
                return restTemplate.postForObject("/upstream", ask, Quote.class);
            }
        }
        "#,
    )]);

    let sym = find(&r, "QuoteController.quote");
    let dirs: Vec<BoundaryDirection> = sym.boundaries.iter().map(|b| b.direction).collect();
    assert!(
        dirs.contains(&BoundaryDirection::Inbound) && dirs.contains(&BoundaryDirection::Outbound),
        "expected both directions, got {:?}",
        sym.boundaries
    );
}

#[test]
fn ordinary_code_is_not_a_boundary() {
    let r = run(&[(
        "src/main/java/com/acme/Math.java",
        r#"
        package com.acme;
        public class Math {
            public int add(int a, int b) { return a + b; }
        }
        "#,
    )]);

    assert!(find(&r, "Math.add").boundaries.is_empty());
    assert!(find(&r, "Math").boundaries.is_empty());
}

#[test]
fn boundaries_survive_into_the_graph() {
    // `graph.json` is one of the two consumers (the agent tools and the
    // canvas read it; the store's facts are the other). A field that stops
    // at `Symbol` would work in `ug query` and be invisible everywhere else.
    let dir = stage(&[(
        "src/main/java/com/acme/PingController.java",
        r#"
        package com.acme;
        import org.springframework.web.bind.annotation.*;
        @RestController
        public class PingController {
            @GetMapping("/ping")
            public String ping() { return "pong"; }
        }
        "#,
    )]);
    let json = index(dir.path().to_string_lossy().to_string());
    let graph: ultragraph::types::GraphData =
        serde_json::from_str(&ultragraph::build_graph(json)).expect("build_graph");

    let node = graph
        .nodes
        .iter()
        .find(|n| n.name == "PingController.ping")
        .expect("handler node");
    assert_eq!(
        node.boundaries.first().map(|b| b.kind.as_str()),
        Some("http.endpoint"),
        "boundaries did not reach the graph node: {:?}",
        node.boundaries
    );
}
