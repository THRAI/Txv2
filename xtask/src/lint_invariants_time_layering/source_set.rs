//! Parsed Rust source-set ownership for public API linting.
//!
//! This module assigns each discovered file a stable crate/module identity and
//! records the explicit module-surface roots selected by a lint rule. Public API
//! classification and inherent-impl resolution consume that topology without
//! flattening sibling files into a synthetic module.

use std::collections::{BTreeMap, BTreeSet};

use crate::Result;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct ModulePath(Vec<String>);

impl ModulePath {
    pub(super) fn child(&self, segment: impl Into<String>) -> Self {
        let mut path = self.0.clone();
        path.push(segment.into());
        Self(path)
    }

    pub(super) fn parent(&self) -> Option<Self> {
        let mut path = self.0.clone();
        path.pop()?;
        Some(Self(path))
    }

    pub(super) fn starts_with(&self, prefix: &Self) -> bool {
        self.0.starts_with(&prefix.0)
    }

    pub(super) fn display(&self) -> String {
        self.0.join("::")
    }
}

pub(super) struct SourceFile {
    pub(super) rel: String,
    pub(super) module: ModulePath,
    pub(super) crate_root: ModulePath,
    pub(super) syntax: syn::File,
}

pub(super) struct SourceSet {
    pub(super) files: Vec<SourceFile>,
    pub(super) surface_roots: BTreeSet<ModulePath>,
}

impl SourceSet {
    pub(super) fn parse(sources: &[(String, String)], surface_specs: &[&str]) -> Result<Self> {
        let mut files = Vec::with_capacity(sources.len());
        let mut owners = BTreeMap::new();
        for (rel, text) in sources {
            let (crate_root, module) = module_identity(rel)?;
            if let Some(previous) = owners.insert(module.clone(), rel.clone()) {
                return Err(format!(
                    "duplicate Rust module identity {} for {previous} and {rel}",
                    module.display()
                ));
            }
            let syntax = syn::parse_file(text)
                .map_err(|err| format!("{rel}: failed to parse Rust source: {err}"))?;
            files.push(SourceFile {
                rel: rel.clone(),
                module,
                crate_root,
                syntax,
            });
        }
        files.sort_by(|left, right| left.rel.cmp(&right.rel));

        let file_modules: BTreeSet<_> = files.iter().map(|file| file.module.clone()).collect();
        let mut surface_roots = BTreeSet::new();
        for spec in surface_specs {
            let candidate = if spec.ends_with('/') {
                format!("{spec}mod.rs")
            } else {
                (*spec).to_owned()
            };
            let Ok((_, module)) = module_identity(&candidate) else {
                continue;
            };
            if file_modules
                .iter()
                .any(|file_module| file_module.starts_with(&module))
            {
                surface_roots.insert(module);
            }
        }

        if !files.is_empty() && surface_roots.is_empty() {
            return Err("public API source set has no matching module-surface root".to_owned());
        }
        Ok(Self {
            files,
            surface_roots,
        })
    }
}

fn module_identity(rel: &str) -> Result<(ModulePath, ModulePath)> {
    let components: Vec<_> = rel
        .split('/')
        .filter(|component| !component.is_empty())
        .collect();
    let Some(src_index) = components.iter().position(|component| *component == "src") else {
        return Err(format!("Rust source is outside a crate src tree: {rel}"));
    };
    if src_index == 0 || src_index + 1 >= components.len() {
        return Err(format!("cannot derive Rust module identity from {rel}"));
    }

    let crate_root = ModulePath(
        components[..src_index]
            .iter()
            .map(|component| (*component).to_owned())
            .collect(),
    );
    let mut module_segments = crate_root.0.clone();
    let source_segments = &components[src_index + 1..];
    let file_name = source_segments
        .last()
        .expect("source path has a file component");
    if !file_name.ends_with(".rs") {
        return Err(format!("public API source is not a Rust file: {rel}"));
    }

    module_segments.extend(
        source_segments[..source_segments.len() - 1]
            .iter()
            .map(|component| (*component).to_owned()),
    );
    match *file_name {
        "lib.rs" | "main.rs" | "mod.rs" => {}
        _ => module_segments.push(file_name.trim_end_matches(".rs").to_owned()),
    }

    Ok((crate_root, ModulePath(module_segments)))
}

#[cfg(test)]
mod tests {
    use super::{module_identity, ModulePath};

    #[test]
    fn file_and_directory_modules_share_their_rust_identity() {
        let (_, flat) = module_identity("crates/example/src/runtime.rs").expect("flat module");
        let (_, grouped) =
            module_identity("crates/example/src/runtime/mod.rs").expect("grouped module");
        assert_eq!(flat, grouped);
        assert_eq!(
            flat,
            ModulePath(vec![
                "crates".to_owned(),
                "example".to_owned(),
                "runtime".to_owned()
            ])
        );
    }

    #[test]
    fn child_files_keep_qualified_module_identity() {
        let (crate_root, child) =
            module_identity("crates/example/src/runtime/api.rs").expect("child module");
        assert_eq!(crate_root.display(), "crates::example");
        assert_eq!(child.display(), "crates::example::runtime::api");
    }
}
