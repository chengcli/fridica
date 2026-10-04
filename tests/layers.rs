use std::{
    fs,
    path::{Path, PathBuf},
};
use syn::{
    visit::{self, Visit},
    UseTree,
};

fn level(name: &str) -> usize {
    match name {
        "core" => 0,
        "config" => 1,
        "store" => 2,
        "exec" | "workers" | "machines" | "github" => 3,
        "attention" | "threads" | "parent" | "slack" | "approvals" => 4,
        "cli" | "control" | "daemon" | "dashboard" | "doctor" | "mcp" | "bin" => 5,
        _ => panic!("unclassified architecture module: {name}"),
    }
}
/// The files that open, migrate or archive the database: the only ones that
/// may name the SQLite store itself (fridica#128). Components hold
/// `store::Shared` and run units of work through the storage contract.
const HOSTS: [&str; 5] = [
    "src/bin/fridica.rs",
    "src/daemon/mod.rs",
    "src/daemon/composition.rs",
    "src/threads/runtime.rs",
    "src/threads/configuration.rs",
];
struct Check {
    module: String,
    path: PathBuf,
    scope: Vec<String>,
    /// Inside a `#[cfg(test)]` module, which may open a store of its own.
    testing: bool,
}
impl Check {
    fn dependency(&self, segments: &[String]) {
        let mut target = self.scope.clone();
        let mut rest = segments;
        match rest.first().map(String::as_str) {
            Some("crate" | "fridica") => {
                target.clear();
                rest = &rest[1..];
            }
            Some("self") => rest = &rest[1..],
            Some("super") => {
                while rest.first().is_some_and(|s| s == "super") {
                    assert!(target.pop().is_some(), "invalid relative module path");
                    rest = &rest[1..];
                }
            }
            _ => return,
        }
        target.extend_from_slice(rest);
        assert!(
            !target.is_empty(),
            "do not alias the crate root in {}",
            self.path.display()
        );
        assert!(
            level(&target[0]) <= level(&self.module),
            "upward dependency in {}: {} -> {}",
            self.path.display(),
            self.module,
            target[0]
        );
        if target.len() >= 2 && target[0] == "store" && target[1] == "Store" && !self.testing {
            assert!(
                HOSTS.iter().any(|host| self.path.ends_with(host)),
                "the SQLite store is named in {}: components hold store::Shared and reach state through the storage contract (fridica#128); only the host opens, migrates and archives the database",
                self.path.display()
            );
        }
    }
    fn tree(&self, tree: &UseTree, mut prefix: Vec<String>) {
        match tree {
            UseTree::Path(p) => {
                prefix.push(p.ident.to_string());
                self.tree(&p.tree, prefix);
            }
            UseTree::Name(n) => {
                prefix.push(n.ident.to_string());
                self.dependency(&prefix);
            }
            UseTree::Rename(n) => {
                prefix.push(n.ident.to_string());
                self.dependency(&prefix);
            }
            UseTree::Glob(_) => self.dependency(&prefix),
            UseTree::Group(g) => {
                for t in &g.items {
                    self.tree(t, prefix.clone());
                }
            }
        }
    }
}
impl<'a> Visit<'a> for Check {
    // Visibility scopes are not imports (pub(crate), pub(super), etc.).
    fn visit_visibility(&mut self, _: &'a syn::Visibility) {}
    fn visit_item_mod(&mut self, node: &'a syn::ItemMod) {
        if node.content.is_some() {
            let testing = self.testing;
            self.testing |= node.attrs.iter().any(|attribute| {
                attribute.path().is_ident("cfg")
                    && attribute
                        .parse_args::<syn::Path>()
                        .is_ok_and(|p| p.is_ident("test"))
            });
            self.scope.push(node.ident.to_string());
            visit::visit_item_mod(self, node);
            self.scope.pop();
            self.testing = testing;
        }
    }
    fn visit_item_use(&mut self, node: &'a syn::ItemUse) {
        self.tree(&node.tree, vec![]);
    }
    fn visit_path(&mut self, node: &'a syn::Path) {
        self.dependency(
            &node
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect::<Vec<_>>(),
        );
        visit::visit_path(self, node);
    }
}
/// SQL and raw database access, as they are written in this codebase
/// (case-sensitive, so prose does not match).
const RAW_STORE: [&str; 13] = [
    "rusqlite",
    "Sqlite(",
    ".transaction()",
    "SELECT ",
    "INSERT INTO",
    "INSERT OR ",
    "REPLACE INTO",
    "UPDATE ",
    "DELETE FROM",
    "CREATE TABLE",
    "CREATE INDEX",
    "DROP TABLE",
    "PRAGMA ",
];
/// The first raw database access in `text`, if any. A closure handed to a
/// `call` is the store's raw connection; `actor.call(&request)` is not.
fn raw_store(text: &str) -> Option<String> {
    let call = regex::Regex::new(r"(\.call\(\s*(move\s*)?\||\bstore\s*\.call\()").unwrap();
    RAW_STORE
        .iter()
        .find(|pattern| text.contains(*pattern))
        .map(|pattern| pattern.to_string())
        .or_else(|| call.find(text).map(|m| m.as_str().to_owned()))
}
fn files(path: &Path, output: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files(&path, output);
        } else if path.extension().is_some_and(|e| e == "rs") {
            output.push(path);
        }
    }
}
#[test]
fn module_dependencies_only_point_downward_and_ddl_is_centralized() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut paths = vec![];
    files(&root, &mut paths);
    for path in paths {
        let relative = path.strip_prefix(&root).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        // Fridica reaches its state only through the storage contract; the
        // store, its SQL and its connection belong to fridica-store-sqlite (#117).
        if let Some(found) = raw_store(&text) {
            panic!(
                "raw database access ({found:?}) in {}: the store belongs to fridica-store-sqlite (#117); use fridica_core::store traits through Store::transact",
                path.display()
            );
        }
        if relative == Path::new("lib.rs") {
            continue;
        }
        let module = relative
            .components()
            .next()
            .unwrap()
            .as_os_str()
            .to_str()
            .unwrap()
            .to_string();
        let syntax = syn::parse_file(&text).unwrap();
        let mut scope: Vec<String> = relative
            .parent()
            .unwrap()
            .components()
            .map(|part| part.as_os_str().to_str().unwrap().to_string())
            .collect();
        let stem = relative.file_stem().unwrap().to_str().unwrap();
        if stem != "mod" {
            scope.push(stem.to_string());
        }
        Check {
            module,
            path: path.clone(),
            scope,
            testing: false,
        }
        .visit_file(&syntax);
        // The schema lives in fridica-store-sqlite (#117).
        {
            for ddl in [
                "CREATE TABLE",
                "ALTER TABLE",
                "DROP TABLE",
                "CREATE INDEX",
                "CREATE TRIGGER",
            ] {
                assert!(
                    !text.contains(ddl),
                    "DDL in fridica (the schema belongs to fridica-store-sqlite) in {}",
                    path.display()
                );
            }
        }
    }
}

#[test]
fn raw_store_access_is_recognised_but_parent_calls_are_not() {
    for source in [
        "store.call(move |c| Ok(()))",
        "self.store\n    .call(|c| Ok(()))",
        "x.call(move|c| f(c))",
        "let tx = c.transaction()?;",
        "Sqlite(&tx).record()",
        "use rusqlite::Connection;",
        "\"SELECT id FROM jobs\"",
        "\"UPDATE jobs SET status='done'\"",
        "\"DELETE FROM jobs\"",
        "\"INSERT INTO jobs VALUES (?)\"",
    ] {
        assert!(raw_store(source).is_some(), "missed: {source}");
    }
    for source in [
        "let (raw, call) = self.call(&request).await?;",
        "actor.call(&request)",
        "// Select the update, then delete it.",
        "store.transact(move |u| u.record(kind, now, payload, true))",
    ] {
        assert_eq!(raw_store(source), None, "flagged: {source}");
    }
}

#[test]
fn relative_imports_cannot_bypass_layers_but_visibility_is_not_an_import() {
    for source in [
        "use crate::threads::actor;",
        "use super::super::threads::actor;",
        "mod nested { use super::super::super::threads::actor; }",
    ] {
        let outcome = std::panic::catch_unwind(|| {
            Check {
                module: "core".into(),
                path: "src/core/example.rs".into(),
                scope: vec!["core".into(), "example".into()],
                testing: false,
            }
            .visit_file(&syn::parse_file(source).unwrap());
        });
        assert!(outcome.is_err(), "allowed forbidden dependency: {source}");
    }
    Check {
        module: "core".into(),
        path: "src/core/example.rs".into(),
        scope: vec!["core".into(), "example".into()],
        testing: false,
    }
    .visit_file(&syn::parse_file("pub(crate) fn f() {} use super::time::Clock;").unwrap());
}

#[test]
fn only_the_host_names_the_sqlite_store() {
    let check = |path: &str, source: &str| {
        std::panic::catch_unwind(|| {
            Check {
                module: "threads".into(),
                path: path.into(),
                scope: vec!["threads".into(), "example".into()],
                testing: false,
            }
            .visit_file(&syn::parse_file(source).unwrap());
        })
    };
    for source in [
        "use crate::store::Store;",
        "use crate::{config::Config, store::Store};",
        "fn open() { crate::store::Store::open(path); }",
    ] {
        assert!(
            check("src/threads/example.rs", source).is_err(),
            "a component named the SQLite store: {source}"
        );
        assert!(
            check("src/threads/runtime.rs", source).is_ok(),
            "the host may name the SQLite store: {source}"
        );
    }
    for source in [
        "use crate::store::Shared;",
        "use crate::store::{archive, Shared};",
        "#[cfg(test)] mod tests { use crate::store::Store; }",
    ] {
        assert!(
            check("src/threads/example.rs", source).is_ok(),
            "flagged: {source}"
        );
    }
}
