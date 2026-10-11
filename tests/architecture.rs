//! Executable dependency rules. These inspect syntax and manifests rather than
//! imposing file-size limits or freezing implementation module names.
use std::{
    fs,
    path::{Path, PathBuf},
};
use syn::visit::Visit;

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn rust_files(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        return vec![path.to_owned()];
    }
    fs::read_dir(path)
        .unwrap()
        .flat_map(|entry| {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path)
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                vec![path]
            } else {
                vec![]
            }
        })
        .collect()
}

#[derive(Default)]
struct Dependencies(Vec<Vec<String>>);

impl Dependencies {
    fn use_tree(&mut self, tree: &syn::UseTree, prefix: &[String]) {
        match tree {
            syn::UseTree::Path(path) => {
                let mut prefix = prefix.to_vec();
                prefix.push(path.ident.to_string());
                self.use_tree(&path.tree, &prefix);
            }
            syn::UseTree::Group(group) => {
                for item in &group.items {
                    self.use_tree(item, prefix);
                }
            }
            syn::UseTree::Name(name) => {
                let mut path = prefix.to_vec();
                path.push(name.ident.to_string());
                self.0.push(path);
            }
            syn::UseTree::Rename(rename) => {
                let mut path = prefix.to_vec();
                path.push(rename.ident.to_string());
                self.0.push(path);
            }
            syn::UseTree::Glob(_) => self.0.push(prefix.to_vec()),
        }
    }
}

impl<'ast> Visit<'ast> for Dependencies {
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.use_tree(&item.tree, &[]);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        self.0.push(
            path.segments
                .iter()
                .map(|part| part.ident.to_string())
                .collect(),
        );
        syn::visit::visit_path(self, path);
    }
}

fn assert_boundaries(path: &Path, forbidden: &[&[&str]]) {
    for file in rust_files(path) {
        let syntax = syn::parse_file(&fs::read_to_string(&file).unwrap())
            .unwrap_or_else(|error| panic!("{}: {error}", file.display()));
        let mut dependencies = Dependencies::default();
        dependencies.visit_file(&syntax);
        for dependency in dependencies.0 {
            for prefix in forbidden {
                assert!(
                    !dependency
                        .iter()
                        .map(String::as_str)
                        .take(prefix.len())
                        .eq(prefix.iter().copied()),
                    "{} crosses an architecture boundary via {}",
                    file.display(),
                    dependency.join("::"),
                );
            }
        }
    }
}

#[test]
fn domain_values_do_not_depend_on_storage_or_adapters() {
    for module in [
        "protocol",
        "execution",
        "memory",
        "code_index",
        "research",
        "schedule",
        "todo",
    ] {
        assert_boundaries(
            &root().join(format!("crates/builder-core/src/{module}.rs")),
            &[
                &["rusqlite"],
                &["crate", "store"],
                &["super", "store"],
                &["reqwest"],
                &["tokio"],
                &["console"],
                &["crossterm"],
                &["axum"],
            ],
        );
    }
}

#[test]
fn application_policy_does_not_depend_on_ui_or_remote_adapters() {
    for module in [
        "agent",
        "memory",
        "code_index",
        "research",
        "embedding.rs",
        "completion.rs",
    ] {
        assert_boundaries(
            &root().join("src").join(module),
            &[
                &["crate", "ui"],
                &["crate", "input"],
                &["crate", "remote"],
                &["crate", "remote_connect"],
                &["console"],
                &["crossterm"],
                &["indicatif"],
                &["axum"],
                &["builder_gateway"],
            ],
        );
    }
    assert_boundaries(
        &root().join("src/remote"),
        &[
            &["crate", "ui"],
            &["crate", "input"],
            &["console"],
            &["crossterm"],
            &["indicatif"],
        ],
    );
    // Code-index policy requires embeddings, never the memory record runtime.
    // Test fixtures may construct MemoryRuntime to check compatibility.
    for file in rust_files(&root().join("src/code_index")) {
        if file.file_name().is_some_and(|name| name != "tests.rs") {
            assert_boundaries(&file, &[&["crate", "memory"]]);
        }
    }
}

#[test]
fn workspace_dependencies_follow_the_inward_rule() {
    for entry in fs::read_dir(root().join("crates")).unwrap() {
        let manifest_path = entry.unwrap().path().join("Cargo.toml");
        let manifest: toml::Value =
            toml::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
        let name = manifest["package"]["name"].as_str().unwrap();
        let allowed: &[&str] = match name {
            "builder-core" | "builder-remote-protocol" => &[],
            "builder-provider" | "builder-tools" | "builder-embedding" => &["builder-core"],
            "builder-gateway" => &["builder-remote-protocol"],
            _ => panic!("Declare the dependency boundary for new crate {name}"),
        };
        let mut tables = vec![manifest.as_table().unwrap()];
        if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
            tables.extend(targets.values().map(|target| target.as_table().unwrap()));
        }
        for table in tables {
            for kind in ["dependencies", "build-dependencies"] {
                if let Some(dependencies) = table.get(kind).and_then(toml::Value::as_table) {
                    for (alias, dependency) in dependencies {
                        let package = dependency
                            .get("package")
                            .and_then(toml::Value::as_str)
                            .unwrap_or(alias);
                        if package == "builder" || package.starts_with("builder-") {
                            assert!(
                                allowed.contains(&package),
                                "{name} must not depend on {package}"
                            );
                        }
                    }
                }
            }
        }
        for key in ["version", "edition", "license", "rust-version"] {
            assert_eq!(
                manifest["package"][key]["workspace"].as_bool(),
                Some(true),
                "{name} must inherit workspace {key}"
            );
        }
    }
}
