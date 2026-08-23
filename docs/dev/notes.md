## Trigger github action to publish a new release
git tag v0.1.4
git push origin v0.1.4


##
the src/lib.rs file name explicitly tells Cargo and the Rust compiler (rustc) to build a library crate.By default, Cargo relies on strict convention-over-configuration. If it sees src/lib.rs in your project folder, it automatically treats it as the entry point (the "crate root") for a library.
(can override lib name/src path in cargo.toml -> [lib] section)
In a rust lib, expose functions, structs, or modules using the pub keyword so external code can see them.
