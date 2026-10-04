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
struct Check {
    module: String,
    path: PathBuf,
    scope: Vec<String>,
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
            self.scope.push(node.ident.to_string());
            visit::visit_item_mod(self, node);
            self.scope.pop();
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
        if relative == Path::new("lib.rs") {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap();
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
            }
            .visit_file(&syn::parse_file(source).unwrap());
        });
        assert!(outcome.is_err(), "allowed forbidden dependency: {source}");
    }
    Check {
        module: "core".into(),
        path: "src/core/example.rs".into(),
        scope: vec!["core".into(), "example".into()],
    }
    .visit_file(&syn::parse_file("pub(crate) fn f() {} use super::time::Clock;").unwrap());
}
