//! Shared scaffolding for turning a file's syntax into *qualified* names.
//!
//! Rust, TypeScript and Python each need the same three things before a call
//! site can be resolved to a definition in another file:
//!
//! 1. the module path the file itself occupies, so its own declarations have
//!    names that are unique repo-wide ([`module_path`]);
//! 2. a map from every name the file imported to the qualified thing that
//!    name refers to ([`ImportScope`]);
//! 3. a running record of which local variables hold which type, so a call
//!    through a receiver knows what it dispatches on ([`TypeEnv`]).
//!
//! Java gets by without any of this because its identity comes from a
//! `package` declaration written *in the source* rather than from where the
//! file sits on disk — see `languages/java.rs`, whose `FileCtx` / `TypeCtx`
//! this module is modelled on.
//!
//! # Why a name that resolves to nothing is the point
//!
//! Every lookup here returns `Option`, and callers are expected to drop the
//! call site when it comes back `None`. That is not a gap to be filled in
//! later — it is the contract. A call to `Vec::push` or `express()` *should*
//! resolve to nothing, because the definition is not in this repo. The
//! failure mode this whole module exists to remove is the opposite one:
//! answering "which repo symbol is this?" with a plausible guess.

use crate::types::ImportInfo;
use std::collections::HashMap;

/// Separator between a type and one of its members, in every language.
///
/// Shared with Java (`pkg.Type#member`) so `graph.rs`'s qualified index needs
/// no per-language branch: only the *module* separator varies.
pub const MEMBER_SEP: char = '#';

/// Callee name recorded for a call site that *constructs* a value rather than
/// invoking a member on one. Shared with Java, which has used this spelling
/// for constructors since before this module existed.
pub const CTOR: &str = "<init>";

/// Module separator for `language`, as written in that language's own paths.
pub fn module_sep(language: &str) -> &'static str {
    match language {
        "rust" => "::",
        // TypeScript module paths stay filesystem-shaped (`src/a/b`), so a
        // dot cannot be confused with a path segment.
        _ => ".",
    }
}

/// The module path a file occupies, in its language's own vocabulary.
///
/// ```text
/// rust        native/src/indexer/languages/rust.rs -> crate::indexer::languages::rust
/// typescript  ui/src/panels/Chat.tsx               -> ui/src/panels/Chat
/// python      pkg/sub/thing.py                     -> pkg.sub.thing
/// ```
///
/// Returns an empty string for languages that don't derive identity from
/// their path (Java, Markdown), which callers treat as "no qualified names
/// for this file".
pub fn module_path(path: &str, language: &str) -> String {
    let stem = strip_extension(path);
    match language {
        "rust" => rust_module_path(stem),
        "typescript" => stem.strip_suffix("/index").unwrap_or(stem).to_string(),
        "python" => stem
            .strip_suffix("/__init__")
            .unwrap_or(stem)
            .replace('/', "."),
        _ => String::new(),
    }
}

/// Drop a trailing file extension, leaving a path with no dot in its last
/// segment. A dotted *directory* name (`my.app/index.ts`) is left alone.
fn strip_extension(path: &str) -> &str {
    match path.rsplit_once('.') {
        Some((stem, ext)) if !ext.contains('/') => stem,
        _ => path,
    }
}

fn rust_module_path(stem: &str) -> String {
    let mut segs: Vec<&str> = stem.split('/').filter(|s| !s.is_empty()).collect();

    // Everything above the crate's `src/` is filesystem layout rather than
    // module structure: `native/src/indexer/languages/rust.rs` is
    // `crate::indexer::languages::rust` however deep the crate is nested.
    if let Some(i) = segs.iter().rposition(|s| *s == "src") {
        segs.drain(..=i);
    }

    // `mod.rs`, `lib.rs` and `main.rs` *are* their parent module rather than
    // a child of it.
    if matches!(segs.last(), Some(&"mod") | Some(&"lib") | Some(&"main")) {
        segs.pop();
    }

    let mut out = String::from("crate");
    for s in segs {
        out.push_str("::");
        out.push_str(s);
    }
    out
}

/// Everything a file imported, keyed by the name it is written as locally.
///
/// Built from the `ImportInfo` list the language indexer already extracts, so
/// no import is parsed twice. A bare specifier that names no local module
/// (`std::collections`, `react`, `os.path`) is recorded too — the qualified
/// name it produces simply matches nothing in the repo-wide index, which is
/// how external calls come to be dropped rather than guessed at.
#[derive(Debug, Clone)]
pub struct ImportScope {
    /// Local name -> qualified target.
    aliases: HashMap<String, String>,
    /// Module path of the file this scope belongs to.
    module: String,
    language: &'static str,
    sep: &'static str,
}

impl ImportScope {
    pub fn new(language: &str, module: String, imports: &[ImportInfo]) -> Self {
        let sep = module_sep(language);
        let language = static_language(language);
        let mut aliases = HashMap::new();

        for import in imports {
            let Some(base) = normalize_specifier(&import.path, &module, language) else {
                continue;
            };
            for item in &import.imported {
                // `use foo::*` / `from foo import *` bring in an unknown set
                // of names. Recording the glob under its own key lets a
                // caller fall back to "somewhere under `base`" without
                // inventing a binding for a name that may not exist.
                if item.name == "*" {
                    aliases.insert(format!("*{}", base), base.clone());
                    continue;
                }
                let local = item.alias.clone().unwrap_or_else(|| item.name.clone());
                let target = if base.is_empty() {
                    item.name.clone()
                } else {
                    format!("{}{}{}", base, sep, item.name)
                };
                aliases.insert(local, target);
            }
        }

        Self {
            aliases,
            module,
            language,
            sep,
        }
    }

    /// Bind a submodule this file *declares* rather than imports — Rust's
    /// `mod cli;`, whose contents live in `cli.rs` or `cli/mod.rs`.
    ///
    /// Without this, `cli::run()` written in the declaring file resolves to
    /// the literal `cli::run` while the declaration is `crate::cli::run`, and
    /// the two never meet. [`resolve_path`](Self::resolve_path) cannot repair
    /// that on its own: its rule for an unbound head segment is to return the
    /// path untouched, which is right for `pkg::mod::Base` (re-rooting it
    /// under the current module would match neither) and wrong here — `cli`
    /// is not another crate, it is this module's child.
    ///
    /// A `mod` declaration is exactly the discriminator Rust itself uses: a
    /// bare `cli::run()` means `self::cli::run` precisely when the file
    /// declares `mod cli;`, and an external crate otherwise. Paths that
    /// genuinely name another crate stay unbound and keep falling through.
    ///
    /// An existing binding wins, so an explicit `use` is never overwritten.
    pub fn declare_child_module(&mut self, name: &str) {
        if name.is_empty() {
            return;
        }
        let target = self.qualify(name);
        self.aliases.entry(name.to_string()).or_insert(target);
    }

    /// Qualified name for something *declared in this file*.
    pub fn qualify(&self, name: &str) -> String {
        if self.module.is_empty() {
            return name.to_string();
        }
        format!("{}{}{}", self.module, self.sep, name)
    }

    /// What a single identifier, as written in this file, refers to.
    ///
    /// Import bindings win over the current module, because a file that
    /// imports `Config` and also declares one is referring to the import at
    /// every site the shadow doesn't cover, and we cannot see scopes.
    pub fn lookup(&self, name: &str) -> Option<String> {
        if let Some(target) = self.aliases.get(name) {
            return Some(target.clone());
        }
        if self.module.is_empty() {
            return None;
        }
        Some(self.qualify(name))
    }

    /// What a scoped path (`a::b::C`, `a.b.C`) written in this file refers to.
    ///
    /// The head segment is resolved through the import bindings and the rest
    /// is appended, so `use crate::agent_tools;` followed by
    /// `agent_tools::find_usages(..)` reaches `crate::agent_tools::find_usages`.
    /// Paths that are already rooted (`crate::`, `self::`, `super::`) are
    /// normalized directly.
    pub fn resolve_path(&self, path: &str) -> Option<String> {
        let path = path.trim();
        if path.is_empty() {
            return None;
        }
        if !path.contains(self.sep) {
            return self.lookup(path);
        }
        // Rust is the only one of the three that writes module paths inline
        // at a call site, so it is the only one with rooted forms to
        // normalize. A dotted path in TypeScript or Python is member access,
        // handled by the head-binding rule below.
        if self.language == "rust" {
            if let Some(rooted) = normalize_specifier(path, &self.module, "rust") {
                if rooted != path {
                    return Some(rooted);
                }
            }
        }
        let (head, rest) = path.split_once(self.sep)?;
        match self.aliases.get(head) {
            Some(base) => Some(format!("{}{}{}", base, self.sep, rest)),
            // No binding for the head segment. The path is already absolute
            // in its language's namespace (`pkg.mod.Base`, `other::thing`),
            // so it is returned unchanged: it will either match the
            // repo-wide index exactly or match nothing, and both are honest
            // answers. What it must not do is get re-rooted under the
            // current module, which would turn `pkg.mod.Base` into
            // `here.pkg.mod.Base` and match neither.
            None => Some(path.to_string()),
        }
    }

    /// Resolve a written type reference — a supertype, an annotation, a
    /// heritage clause — to its qualified name.
    ///
    /// Strips generic parameters first, because `Base<Order>` and `Base[T]`
    /// name the same class as `Base` and neither spelling is in any index.
    pub fn resolve_type_ref(&self, written: &str) -> Option<String> {
        let bare = base_type_name(written);
        if bare.is_empty() {
            return None;
        }
        self.resolve_path(bare)
    }
}

/// Narrow a language name to the `'static` set this module branches on.
fn static_language(language: &str) -> &'static str {
    match language {
        "rust" => "rust",
        "typescript" => "typescript",
        "python" => "python",
        _ => "",
    }
}

/// Turn an import specifier into a repo-wide qualified prefix, or `None` when
/// it plainly names something outside the repo.
///
/// Relative forms are resolved against `module` so that a TypeScript `./util`
/// and a Rust `self::util` land in exactly the namespace the target file's
/// own [`module_path`] produces.
fn normalize_specifier(spec: &str, module: &str, language: &str) -> Option<String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    match language {
        "rust" => {
            if spec == "crate" || spec.starts_with("crate::") {
                return Some(spec.to_string());
            }
            if spec == "self" {
                return Some(module.to_string());
            }
            if let Some(rest) = spec.strip_prefix("self::") {
                return Some(join("::", module, rest));
            }
            let mut ups = 0usize;
            let mut rest = spec;
            while rest == "super" || rest.starts_with("super::") {
                ups += 1;
                match rest.strip_prefix("super::") {
                    Some(r) => rest = r,
                    None => {
                        rest = "";
                        break;
                    }
                }
            }
            if ups > 0 {
                let mut segs: Vec<&str> = module.split("::").collect();
                for _ in 0..ups {
                    if segs.len() > 1 {
                        segs.pop();
                    }
                }
                return Some(join("::", &segs.join("::"), rest));
            }
            // An external crate, or this crate referred to by its published
            // name. Kept verbatim: it will match nothing in the qualified
            // index, and the call site will be dropped.
            Some(spec.to_string())
        }
        "typescript" => {
            if spec.starts_with("./") || spec.starts_with("../") || spec == "." || spec == ".." {
                return resolve_relative_dirs(spec, module, "/");
            }
            // Bare specifier: a package. Left as-is so it matches nothing.
            Some(spec.to_string())
        }
        "python" => {
            if let Some(stripped) = spec.strip_prefix('.') {
                // `from . import x` / `from ..pkg import y`: each leading dot
                // is one level up from the current *package*.
                let mut ups = 1usize;
                let mut rest = stripped;
                while let Some(r) = rest.strip_prefix('.') {
                    ups += 1;
                    rest = r;
                }
                let mut segs: Vec<&str> = module.split('.').collect();
                // A module's own package is its path minus the module itself.
                segs.pop();
                for _ in 1..ups {
                    if !segs.is_empty() {
                        segs.pop();
                    }
                }
                return Some(join(".", &segs.join("."), rest));
            }
            Some(spec.to_string())
        }
        _ => None,
    }
}

/// Resolve a `./`-style specifier against the directory holding `module`.
fn resolve_relative_dirs(spec: &str, module: &str, sep: &str) -> Option<String> {
    let mut segs: Vec<&str> = module.split(sep).collect();
    // Start from the directory the importing file sits in.
    segs.pop();

    for part in spec.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            other => segs.push(other),
        }
    }
    Some(segs.join(sep))
}

fn join(sep: &str, left: &str, right: &str) -> String {
    match (left.is_empty(), right.is_empty()) {
        (true, _) => right.to_string(),
        (_, true) => left.to_string(),
        _ => format!("{}{}{}", left, sep, right),
    }
}

/// Which type each local name currently holds.
///
/// Deliberately flat, matching `java.rs`'s environment: there is no block
/// scoping, so a name redeclared in a sibling block keeps whichever
/// declaration the walk saw last. That is a cheaper mistake than not typing
/// the receiver at all, and it is bounded — the wrong answer is still a type
/// that exists in this file's scope.
#[derive(Debug, Default, Clone)]
pub struct TypeEnv {
    vars: HashMap<String, String>,
}

impl TypeEnv {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: impl Into<String>, qualified_type: impl Into<String>) {
        self.vars.insert(name.into(), qualified_type.into());
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(String::as_str)
    }

}

/// Strip generic arguments and reference/pointer sigils from a written type,
/// leaving the bare name a qualified lookup can be keyed on.
///
/// `&mut Vec<Foo>` -> `Vec`, `Option<Db>` -> `Option`, `Box<dyn Store>` -> `Box`.
pub fn base_type_name(written: &str) -> &str {
    let mut s = written.trim();
    loop {
        let trimmed = s
            .trim_start_matches('&')
            .trim_start_matches("mut ")
            .trim_start_matches("dyn ")
            .trim_start_matches("impl ")
            .trim_start_matches('*')
            .trim();
        if trimmed == s {
            break;
        }
        s = trimmed;
    }
    if let Some(idx) = s.find('<') {
        s = s[..idx].trim_end();
    }
    s
}

/// Whether an identifier looks like a declared constant.
///
/// `SCREAMING_SNAKE_CASE` is the constant convention in Rust, TypeScript and
/// Python alike. Keying on it keeps the reference set small and meaningful:
/// recording *every* identifier a body mentions would bury the graph in
/// edges between locals, and recording none is what left every constant in
/// the repo looking like dead code.
pub fn looks_like_constant(name: &str) -> bool {
    let mut has_letter = false;
    for c in name.chars() {
        if c.is_lowercase() {
            return false;
        }
        if c.is_alphabetic() {
            has_letter = true;
        } else if c != '_' && !c.is_ascii_digit() {
            return false;
        }
    }
    has_letter
}

/// Whether an identifier looks like a type rather than a module or a value.
///
/// Rust, TypeScript and Python all spell types in UpperCamelCase by
/// convention (and, in Rust's case, by a default lint). Java's indexer makes
/// the same call for the same reason — see `invocation_owner` there.
pub fn looks_like_type(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ImportedItem;

    fn import(path: &str, names: &[(&str, Option<&str>)]) -> ImportInfo {
        ImportInfo {
            path: path.to_string(),
            imported: names
                .iter()
                .map(|(n, a)| ImportedItem {
                    name: n.to_string(),
                    alias: a.map(str::to_string),
                })
                .collect(),
        }
    }

    #[test]
    fn rust_module_paths_are_rooted_at_the_crate_not_the_repo() {
        assert_eq!(
            module_path("native/src/indexer/languages/rust.rs", "rust"),
            "crate::indexer::languages::rust"
        );
        assert_eq!(module_path("native/src/graph.rs", "rust"), "crate::graph");
    }

    #[test]
    fn a_rust_module_root_file_is_its_parent_module() {
        assert_eq!(module_path("native/src/lib.rs", "rust"), "crate");
        assert_eq!(module_path("native/src/main.rs", "rust"), "crate");
        assert_eq!(
            module_path("native/src/storage/mod.rs", "rust"),
            "crate::storage"
        );
    }

    #[test]
    fn typescript_and_python_module_paths_follow_their_own_conventions() {
        assert_eq!(module_path("ui/src/panels/Chat.tsx", "typescript"), "ui/src/panels/Chat");
        assert_eq!(module_path("ui/src/panels/index.ts", "typescript"), "ui/src/panels");
        assert_eq!(module_path("pkg/sub/thing.py", "python"), "pkg.sub.thing");
        assert_eq!(module_path("pkg/sub/__init__.py", "python"), "pkg.sub");
    }

    #[test]
    fn a_use_binding_resolves_a_scoped_call_to_its_defining_module() {
        // The exact shape that silently produced no edge before: `use
        // crate::agent_tools;` then `agent_tools::find_usages(..)`.
        let scope = ImportScope::new(
            "rust",
            "crate::mcp".to_string(),
            &[import("crate", &[("agent_tools", None)])],
        );
        assert_eq!(
            scope.resolve_path("agent_tools::find_usages").as_deref(),
            Some("crate::agent_tools::find_usages")
        );
    }

    #[test]
    fn an_aliased_import_resolves_under_its_original_name() {
        let scope = ImportScope::new(
            "rust",
            "crate::a".to_string(),
            &[import("crate::storage::db", &[("Db", Some("Database"))])],
        );
        assert_eq!(
            scope.lookup("Database").as_deref(),
            Some("crate::storage::db::Db")
        );
    }

    #[test]
    fn self_and_super_resolve_against_the_current_module() {
        let scope = ImportScope::new(
            "rust",
            "crate::indexer::languages::rust".to_string(),
            &[
                import("self", &[("helper", None)]),
                import("super", &[("common", None)]),
            ],
        );
        assert_eq!(
            scope.lookup("helper").as_deref(),
            Some("crate::indexer::languages::rust::helper")
        );
        assert_eq!(
            scope.lookup("common").as_deref(),
            Some("crate::indexer::languages::common")
        );
    }

    #[test]
    fn an_unimported_bare_name_is_assumed_to_be_declared_here() {
        let scope = ImportScope::new("rust", "crate::graph".to_string(), &[]);
        assert_eq!(
            scope.lookup("build_graph").as_deref(),
            Some("crate::graph::build_graph")
        );
    }

    #[test]
    fn an_external_crate_keeps_its_own_root_so_it_matches_nothing_local() {
        let scope = ImportScope::new(
            "rust",
            "crate::a".to_string(),
            &[import("std::collections", &[("HashMap", None)])],
        );
        assert_eq!(
            scope.lookup("HashMap").as_deref(),
            Some("std::collections::HashMap")
        );
    }

    #[test]
    fn typescript_relative_imports_land_in_the_target_files_own_namespace() {
        let scope = ImportScope::new(
            "typescript",
            "ui/src/panels/Chat".to_string(),
            &[
                import("./util", &[("format", None)]),
                import("../store", &[("Store", None)]),
            ],
        );
        assert_eq!(
            scope.lookup("format").as_deref(),
            Some("ui/src/panels/util.format")
        );
        assert_eq!(
            scope.lookup("Store").as_deref(),
            Some("ui/src/store.Store")
        );
    }

    #[test]
    fn python_relative_imports_resolve_against_the_package_not_the_module() {
        let scope = ImportScope::new(
            "python",
            "pkg.sub.thing".to_string(),
            &[
                import(".helper", &[("run", None)]),
                import("..other", &[("Thing", None)]),
            ],
        );
        assert_eq!(scope.lookup("run").as_deref(), Some("pkg.sub.helper.run"));
        assert_eq!(scope.lookup("Thing").as_deref(), Some("pkg.other.Thing"));
    }

    #[test]
    fn generic_and_reference_sigils_are_stripped_from_written_types() {
        assert_eq!(base_type_name("&mut Vec<Foo>"), "Vec");
        assert_eq!(base_type_name("Option<Db>"), "Option");
        assert_eq!(base_type_name("Box<dyn Store>"), "Box");
        assert_eq!(base_type_name("  Db  "), "Db");
    }

    #[test]
    fn a_type_env_keeps_the_last_declaration_of_a_shadowed_name() {
        let mut env = TypeEnv::new();
        env.insert("x", "crate::a::Foo");
        env.insert("x", "crate::a::Bar");
        assert_eq!(env.get("x"), Some("crate::a::Bar"));
        assert_eq!(env.get("nope"), None);
    }
}
