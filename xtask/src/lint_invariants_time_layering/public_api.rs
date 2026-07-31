//! Externally reachable Rust API signature scanning for time-layer rules.
//!
//! The first pass indexes modules, local nominal types, and rooted import aliases
//! across the complete discovered source set. The second pass scans only syntax
//! that contributes to externally reachable API: item headers, generic bounds,
//! declared types, public fields, enum variant fields, trait signatures, and
//! public inherent items whose receiver resolves to one qualified public type.
//! Attributes, expressions, blocks, private fields, and trait impls are never
//! part of the scanned token stream.

use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::{Span, TokenStream, TokenTree};
use quote::ToTokens;
use syn::{
    Field, Fields, ForeignItem, ImplItem, Item, ItemImpl, ItemUse, TraitItem, Type, UseTree,
    Visibility,
};

use crate::Result;

use super::source_set::{ModulePath, SourceSet};

type FlatToken = (String, Span);
type TypeRegistry = BTreeMap<ModulePath, bool>;
type ImportMap = BTreeMap<(ModulePath, String), BTreeSet<ModulePath>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PublicItemHit {
    pub(super) rel: String,
    pub(super) line: usize,
}

#[cfg(test)]
pub(super) fn externally_public_item_hits(
    text: &str,
    needles: &[&str],
) -> Result<Vec<PublicItemHit>> {
    externally_public_source_hits(
        &[("fixture/src/lib.rs".to_owned(), text.to_owned())],
        &["fixture/src/lib.rs"],
        needles,
    )
}

pub(super) fn externally_public_source_hits(
    sources: &[(String, String)],
    surface_specs: &[&str],
    needles: &[&str],
) -> Result<Vec<PublicItemHit>> {
    let source_set = SourceSet::parse(sources, surface_specs)?;
    let needle_tokens = needles
        .iter()
        .map(|needle| {
            needle
                .parse::<TokenStream>()
                .map(flatten_nonliteral_tokens)
                .map(|tokens| tokens.into_iter().map(|(token, _)| token).collect())
                .map_err(|err| format!("invalid public-item needle {needle:?}: {err}"))
        })
        .collect::<Result<Vec<Vec<String>>>>()?;

    let module_edges = collect_module_edges(&source_set)?;
    let reachable_modules = reachable_modules(&source_set, &module_edges);
    let types = collect_local_types(&source_set, &reachable_modules)?;
    let imports = collect_local_imports(&source_set)?;

    let mut hits = Vec::new();
    for source in &source_set.files {
        let mut hit_lines = BTreeSet::new();
        collect_public_api_hits(
            &source.syntax.items,
            &source.module,
            &source.crate_root,
            reachable_modules.contains(&source.module),
            &types,
            &imports,
            &needle_tokens,
            &mut hit_lines,
        )?;
        hits.extend(hit_lines.into_iter().map(|line| PublicItemHit {
            rel: source.rel.clone(),
            line,
        }));
    }
    Ok(hits)
}

fn collect_module_edges(source_set: &SourceSet) -> Result<BTreeMap<ModulePath, bool>> {
    let mut edges = BTreeMap::new();
    for source in &source_set.files {
        collect_module_edges_in_items(&source.syntax.items, &source.module, &mut edges)?;
    }
    Ok(edges)
}

fn collect_module_edges_in_items(
    items: &[Item],
    scope: &ModulePath,
    edges: &mut BTreeMap<ModulePath, bool>,
) -> Result<()> {
    for item in items {
        if let Item::Mod(item) = item {
            let child = scope.child(item.ident.to_string());
            let is_public = matches!(item.vis, Visibility::Public(_));
            if let Some(previous) = edges.insert(child.clone(), is_public) {
                if previous != is_public {
                    return Err(format!(
                        "conflicting visibility declarations for module {}",
                        child.display()
                    ));
                }
            }
            if let Some((_, children)) = &item.content {
                collect_module_edges_in_items(children, &child, edges)?;
            }
        }
    }
    Ok(())
}

fn reachable_modules(
    source_set: &SourceSet,
    module_edges: &BTreeMap<ModulePath, bool>,
) -> BTreeSet<ModulePath> {
    let mut reachable = source_set.surface_roots.clone();
    loop {
        let mut changed = false;
        for (child, is_public) in module_edges {
            if !is_public || reachable.contains(child) {
                continue;
            }
            if child
                .parent()
                .is_some_and(|parent| reachable.contains(&parent))
            {
                changed |= reachable.insert(child.clone());
            }
        }
        if !changed {
            return reachable;
        }
    }
}

fn collect_local_types(
    source_set: &SourceSet,
    reachable_modules: &BTreeSet<ModulePath>,
) -> Result<TypeRegistry> {
    let mut types = BTreeMap::new();
    for source in &source_set.files {
        collect_local_types_in_items(
            &source.syntax.items,
            &source.module,
            reachable_modules,
            &mut types,
        )?;
    }
    Ok(types)
}

fn collect_local_types_in_items(
    items: &[Item],
    scope: &ModulePath,
    reachable_modules: &BTreeSet<ModulePath>,
    types: &mut TypeRegistry,
) -> Result<()> {
    for item in items {
        let nominal = match item {
            Item::Struct(item) => Some((&item.ident, &item.vis)),
            Item::Enum(item) => Some((&item.ident, &item.vis)),
            Item::Union(item) => Some((&item.ident, &item.vis)),
            _ => None,
        };
        if let Some((ident, visibility)) = nominal {
            let qualified = scope.child(ident.to_string());
            let is_public =
                reachable_modules.contains(scope) && matches!(visibility, Visibility::Public(_));
            if types.insert(qualified.clone(), is_public).is_some() {
                return Err(format!(
                    "duplicate local nominal type {}",
                    qualified.display()
                ));
            }
        }
        if let Item::Mod(item) = item {
            if let Some((_, children)) = &item.content {
                collect_local_types_in_items(
                    children,
                    &scope.child(item.ident.to_string()),
                    reachable_modules,
                    types,
                )?;
            }
        }
    }
    Ok(())
}

fn collect_local_imports(source_set: &SourceSet) -> Result<ImportMap> {
    let mut imports = BTreeMap::new();
    for source in &source_set.files {
        collect_local_imports_in_items(
            &source.syntax.items,
            &source.module,
            &source.crate_root,
            &mut imports,
        )?;
    }
    Ok(imports)
}

fn collect_local_imports_in_items(
    items: &[Item],
    scope: &ModulePath,
    crate_root: &ModulePath,
    imports: &mut ImportMap,
) -> Result<()> {
    for item in items {
        match item {
            Item::Use(item) => collect_use_aliases(item, scope, crate_root, imports)?,
            Item::Mod(item) => {
                if let Some((_, children)) = &item.content {
                    collect_local_imports_in_items(
                        children,
                        &scope.child(item.ident.to_string()),
                        crate_root,
                        imports,
                    )?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn collect_use_aliases(
    item: &ItemUse,
    scope: &ModulePath,
    crate_root: &ModulePath,
    imports: &mut ImportMap,
) -> Result<()> {
    if item.leading_colon.is_some() {
        return Ok(());
    }
    let mut leaves = Vec::new();
    flatten_use_tree(&item.tree, &mut Vec::new(), &mut leaves);
    for (segments, alias) in leaves {
        let Some(target) = resolve_rooted_segments(&segments, scope, crate_root)? else {
            continue;
        };
        imports
            .entry((scope.clone(), alias))
            .or_default()
            .insert(target);
    }
    Ok(())
}

fn flatten_use_tree(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    leaves: &mut Vec<(Vec<String>, String)>,
) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            flatten_use_tree(&path.tree, prefix, leaves);
            prefix.pop();
        }
        UseTree::Name(name) if name.ident == "self" => {
            if let Some(alias) = prefix.last() {
                leaves.push((prefix.clone(), alias.clone()));
            }
        }
        UseTree::Name(name) => {
            let mut path = prefix.clone();
            path.push(name.ident.to_string());
            leaves.push((path, name.ident.to_string()));
        }
        UseTree::Rename(rename) => {
            let mut path = prefix.clone();
            path.push(rename.ident.to_string());
            leaves.push((path, rename.rename.to_string()));
        }
        UseTree::Group(group) => {
            for item in &group.items {
                flatten_use_tree(item, prefix, leaves);
            }
        }
        UseTree::Glob(_) => {}
    }
}

fn resolve_rooted_segments(
    segments: &[String],
    scope: &ModulePath,
    crate_root: &ModulePath,
) -> Result<Option<ModulePath>> {
    let Some(first) = segments.first().map(String::as_str) else {
        return Ok(None);
    };
    let mut index = 0;
    let mut resolved = match first {
        "crate" => {
            index = 1;
            crate_root.clone()
        }
        "self" => {
            index = 1;
            scope.clone()
        }
        "super" => scope.clone(),
        _ => return Ok(None),
    };
    while segments
        .get(index)
        .is_some_and(|segment| segment == "super")
    {
        if resolved == *crate_root {
            return Err(format!(
                "module path escapes crate root from {}",
                scope.display()
            ));
        }
        resolved = resolved
            .parent()
            .ok_or_else(|| format!("module path escapes source root from {}", scope.display()))?;
        index += 1;
    }
    for segment in &segments[index..] {
        resolved = resolved.child(segment.clone());
    }
    Ok(Some(resolved))
}

fn collect_public_api_hits(
    items: &[Item],
    scope: &ModulePath,
    crate_root: &ModulePath,
    parent_is_public: bool,
    types: &TypeRegistry,
    imports: &ImportMap,
    needles: &[Vec<String>],
    hit_lines: &mut BTreeSet<usize>,
) -> Result<()> {
    for item in items {
        match item {
            Item::Const(item) => {
                if is_externally_public(&item.vis, parent_is_public) {
                    let mut tokens = TokenStream::new();
                    item.vis.to_tokens(&mut tokens);
                    item.const_token.to_tokens(&mut tokens);
                    item.ident.to_tokens(&mut tokens);
                    item.generics.to_tokens(&mut tokens);
                    item.colon_token.to_tokens(&mut tokens);
                    item.ty.to_tokens(&mut tokens);
                    record_fragment(tokens, needles, hit_lines);
                }
            }
            Item::Enum(item) => {
                if is_externally_public(&item.vis, parent_is_public) {
                    let mut header = TokenStream::new();
                    item.vis.to_tokens(&mut header);
                    item.enum_token.to_tokens(&mut header);
                    item.ident.to_tokens(&mut header);
                    item.generics.to_tokens(&mut header);
                    record_fragment(header, needles, hit_lines);
                    for variant in &item.variants {
                        let mut variant_name = TokenStream::new();
                        variant.ident.to_tokens(&mut variant_name);
                        record_fragment(variant_name, needles, hit_lines);
                        scan_fields(&variant.fields, true, needles, hit_lines);
                    }
                }
            }
            Item::ExternCrate(item) => {
                if is_externally_public(&item.vis, parent_is_public) {
                    let mut tokens = TokenStream::new();
                    item.vis.to_tokens(&mut tokens);
                    item.extern_token.to_tokens(&mut tokens);
                    item.crate_token.to_tokens(&mut tokens);
                    item.ident.to_tokens(&mut tokens);
                    if let Some((as_token, rename)) = &item.rename {
                        as_token.to_tokens(&mut tokens);
                        rename.to_tokens(&mut tokens);
                    }
                    record_fragment(tokens, needles, hit_lines);
                }
            }
            Item::Fn(item) => {
                if is_externally_public(&item.vis, parent_is_public) {
                    let mut tokens = TokenStream::new();
                    item.vis.to_tokens(&mut tokens);
                    item.sig.to_tokens(&mut tokens);
                    record_fragment(tokens, needles, hit_lines);
                }
            }
            Item::ForeignMod(item) => {
                if parent_is_public {
                    scan_foreign_items(&item.items, needles, hit_lines)?;
                }
            }
            Item::Impl(item) => {
                if item.trait_.is_none() {
                    scan_inherent_impl(
                        item, scope, crate_root, types, imports, needles, hit_lines,
                    )?;
                }
            }
            Item::Macro(item) => {
                let exported = item
                    .attrs
                    .iter()
                    .any(|attribute| attribute.path().is_ident("macro_export"));
                if parent_is_public && (exported || item.ident.is_none()) {
                    return Err(format!(
                        "unsupported externally reachable macro item in module {}",
                        display_scope(scope)
                    ));
                }
            }
            Item::Mod(item) => {
                let module_is_public = is_externally_public(&item.vis, parent_is_public);
                if module_is_public {
                    let mut tokens = TokenStream::new();
                    item.vis.to_tokens(&mut tokens);
                    item.unsafety.to_tokens(&mut tokens);
                    item.mod_token.to_tokens(&mut tokens);
                    item.ident.to_tokens(&mut tokens);
                    record_fragment(tokens, needles, hit_lines);
                }
                if let Some((_, children)) = &item.content {
                    let child_scope = scope.child(item.ident.to_string());
                    collect_public_api_hits(
                        children,
                        &child_scope,
                        crate_root,
                        module_is_public,
                        types,
                        imports,
                        needles,
                        hit_lines,
                    )?;
                }
            }
            Item::Static(item) => {
                if is_externally_public(&item.vis, parent_is_public) {
                    let mut tokens = TokenStream::new();
                    item.vis.to_tokens(&mut tokens);
                    item.static_token.to_tokens(&mut tokens);
                    item.mutability.to_tokens(&mut tokens);
                    item.ident.to_tokens(&mut tokens);
                    item.colon_token.to_tokens(&mut tokens);
                    item.ty.to_tokens(&mut tokens);
                    record_fragment(tokens, needles, hit_lines);
                }
            }
            Item::Struct(item) => {
                if is_externally_public(&item.vis, parent_is_public) {
                    let mut header = TokenStream::new();
                    item.vis.to_tokens(&mut header);
                    item.struct_token.to_tokens(&mut header);
                    item.ident.to_tokens(&mut header);
                    item.generics.to_tokens(&mut header);
                    record_fragment(header, needles, hit_lines);
                    scan_fields(&item.fields, false, needles, hit_lines);
                }
            }
            Item::Trait(item) => {
                if is_externally_public(&item.vis, parent_is_public) {
                    if item.restriction.is_some() {
                        return Err(format!(
                            "unsupported restricted public trait in module {}",
                            display_scope(scope)
                        ));
                    }
                    let mut header = TokenStream::new();
                    item.vis.to_tokens(&mut header);
                    item.unsafety.to_tokens(&mut header);
                    item.auto_token.to_tokens(&mut header);
                    item.trait_token.to_tokens(&mut header);
                    item.ident.to_tokens(&mut header);
                    item.generics.to_tokens(&mut header);
                    item.colon_token.to_tokens(&mut header);
                    item.supertraits.to_tokens(&mut header);
                    record_fragment(header, needles, hit_lines);
                    scan_trait_items(&item.items, scope, needles, hit_lines)?;
                }
            }
            Item::TraitAlias(item) => {
                if is_externally_public(&item.vis, parent_is_public) {
                    let mut tokens = TokenStream::new();
                    item.vis.to_tokens(&mut tokens);
                    item.trait_token.to_tokens(&mut tokens);
                    item.ident.to_tokens(&mut tokens);
                    item.generics.to_tokens(&mut tokens);
                    item.eq_token.to_tokens(&mut tokens);
                    item.bounds.to_tokens(&mut tokens);
                    record_fragment(tokens, needles, hit_lines);
                }
            }
            Item::Type(item) => {
                if is_externally_public(&item.vis, parent_is_public) {
                    let mut tokens = TokenStream::new();
                    item.vis.to_tokens(&mut tokens);
                    item.type_token.to_tokens(&mut tokens);
                    item.ident.to_tokens(&mut tokens);
                    item.generics.to_tokens(&mut tokens);
                    item.eq_token.to_tokens(&mut tokens);
                    item.ty.to_tokens(&mut tokens);
                    record_fragment(tokens, needles, hit_lines);
                }
            }
            Item::Union(item) => {
                if is_externally_public(&item.vis, parent_is_public) {
                    let mut header = TokenStream::new();
                    item.vis.to_tokens(&mut header);
                    item.union_token.to_tokens(&mut header);
                    item.ident.to_tokens(&mut header);
                    item.generics.to_tokens(&mut header);
                    record_fragment(header, needles, hit_lines);
                    for field in &item.fields.named {
                        scan_field(field, false, needles, hit_lines);
                    }
                }
            }
            Item::Use(item) => {
                if is_externally_public(&item.vis, parent_is_public) {
                    let mut tokens = TokenStream::new();
                    item.vis.to_tokens(&mut tokens);
                    item.use_token.to_tokens(&mut tokens);
                    item.leading_colon.to_tokens(&mut tokens);
                    item.tree.to_tokens(&mut tokens);
                    record_fragment(tokens, needles, hit_lines);
                }
            }
            Item::Verbatim(tokens) => {
                if parent_is_public && !tokens.is_empty() {
                    return Err(format!(
                        "unsupported externally reachable verbatim item in module {}",
                        display_scope(scope)
                    ));
                }
            }
            _ => {
                if parent_is_public {
                    return Err(format!(
                        "unsupported Rust item kind in externally reachable module {}",
                        display_scope(scope)
                    ));
                }
            }
        }
    }
    Ok(())
}

fn scan_fields(
    fields: &Fields,
    enum_fields_are_public: bool,
    needles: &[Vec<String>],
    hit_lines: &mut BTreeSet<usize>,
) {
    match fields {
        Fields::Named(fields) => {
            for field in &fields.named {
                scan_field(field, enum_fields_are_public, needles, hit_lines);
            }
        }
        Fields::Unnamed(fields) => {
            for field in &fields.unnamed {
                scan_field(field, enum_fields_are_public, needles, hit_lines);
            }
        }
        Fields::Unit => {}
    }
}

fn scan_field(
    field: &Field,
    inherited_public: bool,
    needles: &[Vec<String>],
    hit_lines: &mut BTreeSet<usize>,
) {
    if !inherited_public && !matches!(field.vis, Visibility::Public(_)) {
        return;
    }
    let mut tokens = TokenStream::new();
    field.vis.to_tokens(&mut tokens);
    field.ident.to_tokens(&mut tokens);
    field.colon_token.to_tokens(&mut tokens);
    field.ty.to_tokens(&mut tokens);
    record_fragment(tokens, needles, hit_lines);
}

fn scan_trait_items(
    items: &[TraitItem],
    scope: &ModulePath,
    needles: &[Vec<String>],
    hit_lines: &mut BTreeSet<usize>,
) -> Result<()> {
    for item in items {
        let mut tokens = TokenStream::new();
        match item {
            TraitItem::Const(item) => {
                item.const_token.to_tokens(&mut tokens);
                item.ident.to_tokens(&mut tokens);
                item.generics.to_tokens(&mut tokens);
                item.colon_token.to_tokens(&mut tokens);
                item.ty.to_tokens(&mut tokens);
            }
            TraitItem::Fn(item) => item.sig.to_tokens(&mut tokens),
            TraitItem::Type(item) => {
                item.type_token.to_tokens(&mut tokens);
                item.ident.to_tokens(&mut tokens);
                item.generics.to_tokens(&mut tokens);
                item.colon_token.to_tokens(&mut tokens);
                item.bounds.to_tokens(&mut tokens);
                if let Some((eq_token, ty)) = &item.default {
                    eq_token.to_tokens(&mut tokens);
                    ty.to_tokens(&mut tokens);
                }
            }
            TraitItem::Macro(_) | TraitItem::Verbatim(_) => {
                return Err(format!(
                    "unsupported public trait item in module {}",
                    display_scope(scope)
                ));
            }
            _ => {
                return Err(format!(
                    "unsupported public trait item kind in module {}",
                    display_scope(scope)
                ));
            }
        }
        record_fragment(tokens, needles, hit_lines);
    }
    Ok(())
}

fn scan_foreign_items(
    items: &[ForeignItem],
    needles: &[Vec<String>],
    hit_lines: &mut BTreeSet<usize>,
) -> Result<()> {
    for item in items {
        let mut tokens = TokenStream::new();
        match item {
            ForeignItem::Fn(item) if matches!(item.vis, Visibility::Public(_)) => {
                item.vis.to_tokens(&mut tokens);
                item.sig.to_tokens(&mut tokens);
            }
            ForeignItem::Static(item) if matches!(item.vis, Visibility::Public(_)) => {
                item.vis.to_tokens(&mut tokens);
                item.static_token.to_tokens(&mut tokens);
                item.mutability.to_tokens(&mut tokens);
                item.ident.to_tokens(&mut tokens);
                item.colon_token.to_tokens(&mut tokens);
                item.ty.to_tokens(&mut tokens);
            }
            ForeignItem::Type(item) if matches!(item.vis, Visibility::Public(_)) => {
                item.vis.to_tokens(&mut tokens);
                item.type_token.to_tokens(&mut tokens);
                item.ident.to_tokens(&mut tokens);
                item.generics.to_tokens(&mut tokens);
            }
            ForeignItem::Fn(_) | ForeignItem::Static(_) | ForeignItem::Type(_) => continue,
            ForeignItem::Macro(_) | ForeignItem::Verbatim(_) => {
                return Err("unsupported foreign macro or verbatim item in public scope".to_owned());
            }
            _ => return Err("unsupported foreign item kind in public scope".to_owned()),
        }
        record_fragment(tokens, needles, hit_lines);
    }
    Ok(())
}

fn scan_inherent_impl(
    item: &ItemImpl,
    scope: &ModulePath,
    crate_root: &ModulePath,
    types: &TypeRegistry,
    imports: &ImportMap,
    needles: &[Vec<String>],
    hit_lines: &mut BTreeSet<usize>,
) -> Result<()> {
    if !item.items.iter().any(impl_item_is_public) {
        return Ok(());
    }
    let target = resolve_local_type_path(&item.self_ty, scope, crate_root, types, imports)?;
    if !types.get(&target).copied().unwrap_or(false) {
        return Ok(());
    }

    let mut generics = TokenStream::new();
    item.generics.to_tokens(&mut generics);
    record_fragment(generics, needles, hit_lines);

    for associated in &item.items {
        let mut tokens = TokenStream::new();
        match associated {
            ImplItem::Const(item) if matches!(item.vis, Visibility::Public(_)) => {
                item.vis.to_tokens(&mut tokens);
                item.const_token.to_tokens(&mut tokens);
                item.ident.to_tokens(&mut tokens);
                item.generics.to_tokens(&mut tokens);
                item.colon_token.to_tokens(&mut tokens);
                item.ty.to_tokens(&mut tokens);
            }
            ImplItem::Fn(item) if matches!(item.vis, Visibility::Public(_)) => {
                item.vis.to_tokens(&mut tokens);
                item.sig.to_tokens(&mut tokens);
            }
            ImplItem::Type(item) if matches!(item.vis, Visibility::Public(_)) => {
                item.vis.to_tokens(&mut tokens);
                item.type_token.to_tokens(&mut tokens);
                item.ident.to_tokens(&mut tokens);
                item.generics.to_tokens(&mut tokens);
                item.eq_token.to_tokens(&mut tokens);
                item.ty.to_tokens(&mut tokens);
            }
            ImplItem::Const(_) | ImplItem::Fn(_) | ImplItem::Type(_) => continue,
            ImplItem::Macro(_) | ImplItem::Verbatim(_) => {
                return Err(format!(
                    "unsupported inherent macro or verbatim item on public type {}",
                    target.display()
                ));
            }
            _ => {
                return Err(format!(
                    "unsupported inherent item kind on public type {}",
                    target.display()
                ));
            }
        }
        record_fragment(tokens, needles, hit_lines);
    }
    Ok(())
}

fn impl_item_is_public(item: &ImplItem) -> bool {
    match item {
        ImplItem::Const(item) => matches!(item.vis, Visibility::Public(_)),
        ImplItem::Fn(item) => matches!(item.vis, Visibility::Public(_)),
        ImplItem::Type(item) => matches!(item.vis, Visibility::Public(_)),
        ImplItem::Macro(_) | ImplItem::Verbatim(_) => false,
        _ => false,
    }
}

fn resolve_local_type_path(
    ty: &Type,
    scope: &ModulePath,
    crate_root: &ModulePath,
    types: &TypeRegistry,
    imports: &ImportMap,
) -> Result<ModulePath> {
    let ty = match ty {
        Type::Group(group) => &*group.elem,
        Type::Paren(paren) => &*paren.elem,
        ty => ty,
    };
    let Type::Path(path) = ty else {
        return Err(format!(
            "unsupported inherent impl receiver in module {}",
            scope.display()
        ));
    };
    if path.qself.is_some() {
        return Err(format!(
            "unsupported qualified inherent impl receiver in module {}",
            scope.display()
        ));
    }

    let segments: Vec<_> = path
        .path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect();
    if segments.is_empty() {
        return Err(format!(
            "empty inherent impl receiver in module {}",
            scope.display()
        ));
    }
    if path.path.leading_colon.is_some() {
        return Err(format!(
            "absolute external inherent impl receiver is unsupported in module {}",
            scope.display()
        ));
    }

    if let Some(rooted) = resolve_rooted_segments(&segments, scope, crate_root)? {
        if types.contains_key(&rooted) {
            return Ok(rooted);
        }
        return Err(format!(
            "unresolved inherent impl receiver {} in module {}",
            rooted.display(),
            scope.display()
        ));
    }

    let alias_key = (scope.clone(), segments[0].clone());
    if let Some(targets) = imports.get(&alias_key) {
        if targets.len() != 1 {
            return Err(format!(
                "ambiguous inherent impl receiver alias {} in module {}: {}",
                segments[0],
                scope.display(),
                targets
                    .iter()
                    .map(ModulePath::display)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let mut target = targets.iter().next().expect("one alias target").clone();
        for segment in &segments[1..] {
            target = target.child(segment.clone());
        }
        if types.contains_key(&target) {
            return Ok(target);
        }
        return Err(format!(
            "inherent impl receiver alias {} resolves to non-type {} in module {}",
            segments[0],
            target.display(),
            scope.display()
        ));
    }

    let mut local = scope.clone();
    for segment in &segments {
        local = local.child(segment.clone());
    }
    if types.contains_key(&local) {
        return Ok(local);
    }
    let mut rendered = TokenStream::new();
    ty.to_tokens(&mut rendered);
    Err(format!(
        "unresolved inherent impl receiver `{rendered}` in module {}",
        scope.display()
    ))
}

fn record_fragment(tokens: TokenStream, needles: &[Vec<String>], hit_lines: &mut BTreeSet<usize>) {
    let item_tokens = flatten_nonliteral_tokens(tokens);
    for needle in needles {
        let Some(start) = item_tokens
            .windows(needle.len())
            .position(|window| window.iter().map(|(token, _)| token).eq(needle.iter()))
        else {
            continue;
        };
        hit_lines.insert(item_tokens[start].1.start().line);
        return;
    }
}

fn flatten_nonliteral_tokens(tokens: TokenStream) -> Vec<FlatToken> {
    fn flatten(tokens: TokenStream, flattened: &mut Vec<FlatToken>) {
        for token in tokens {
            match token {
                TokenTree::Group(group) => flatten(group.stream(), flattened),
                TokenTree::Ident(ident) => flattened.push((ident.to_string(), ident.span())),
                TokenTree::Punct(punct) => {
                    flattened.push((punct.as_char().to_string(), punct.span()))
                }
                TokenTree::Literal(_) => {}
            }
        }
    }

    let mut flattened = Vec::new();
    flatten(tokens, &mut flattened);
    flattened
}

fn is_externally_public(visibility: &Visibility, parent_is_public: bool) -> bool {
    parent_is_public && matches!(visibility, Visibility::Public(_))
}

fn display_scope(scope: &ModulePath) -> String {
    scope.display()
}
