//! Resident-root RCU ratchet for PageContainer.
//!
//! This is deliberately source and AST based: the resident publication layout
//! is a cross-crate ownership boundary, so a host test alone cannot prevent a
//! later staging regression from putting mutable authority back on the hit
//! path.

use std::fs;
use std::path::Path;

use syn::{Field, Fields, Item, ItemStruct, Type};

use crate::Result;

const PAGE_BACKED: &str = "crates/tx-subsystems/src/page_backed/mod.rs";
const RESIDENT: &str = "crates/tx-subsystems/src/page_backed/resident.rs";
const MANAGER: &str = "crates/tx-subsystems/src/io_manager/page/manager.rs";
const BLOCK_RUNTIME: &str = "crates/tx-subsystems/src/page_backed/block_runtime.rs";
const ASYNC_STATE_SOURCES: &[&str] = &[
    "crates/tx-subsystems/src/page_backed/mod.rs",
    "crates/tx-subsystems/src/page_backed/resident.rs",
    "crates/tx-subsystems/src/page_backed/lifecycle.rs",
    "crates/tx-subsystems/src/page_backed/direct_io.rs",
    "crates/tx-subsystems/src/page_backed/fsync_submission.rs",
    "crates/tx-subsystems/src/page_backed/range.rs",
    "crates/tx-subsystems/src/page_backed/gift.rs",
    "crates/tx-subsystems/src/page_backed/reflink.rs",
    "crates/tx-subsystems/src/io_manager/page/manager.rs",
    "crates/tx-subsystems/src/page_backed/block_runtime.rs",
];

pub(crate) fn lint_pagecontainer_resident_rcu(root: &Path) -> Result<()> {
    let page_backed = read(root, PAGE_BACKED)?;
    let resident = read(root, RESIDENT)?;
    let manager = read(root, MANAGER)?;
    let block_runtime = read(root, BLOCK_RUNTIME)?;
    let async_sources = ASYNC_STATE_SOURCES
        .iter()
        .map(|path| read(root, path).map(|source| ((*path, source))))
        .collect::<Result<Vec<_>>>()?;

    let mut findings = Vec::new();
    findings.extend(resident_shape_findings(RESIDENT, &resident)?);
    findings.extend(page_container_shape_findings(PAGE_BACKED, &page_backed)?);
    findings.extend(manager_shape_findings(
        MANAGER,
        &manager,
        "PageIoSubmissionManager",
    )?);
    findings.extend(manager_shape_findings(
        BLOCK_RUNTIME,
        &block_runtime,
        "BlockSubmissionManager",
    )?);
    findings.extend(hit_path_findings(PAGE_BACKED, &page_backed));
    findings.extend(retirement_findings(PAGE_BACKED, &page_backed));
    for (path, source) in &async_sources {
        findings.extend(guard_bearing_async_state_findings(path, source)?);
    }

    println!("Invariants Lint — pagecontainer-resident-rcu");
    println!("=============================================");
    if findings.is_empty() {
        println!("resident root, L4/L6 ownership, and hit path: ok");
        Ok(())
    } else {
        for finding in &findings {
            eprintln!("  {finding}");
        }
        Err(format!(
            "pagecontainer-resident-rcu ratchet found {} issue(s)",
            findings.len()
        ))
    }
}

fn read(root: &Path, relative: &str) -> Result<String> {
    let path = root.join(relative);
    fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))
}

fn parse(relative: &str, source: &str) -> Result<syn::File> {
    syn::parse_file(source).map_err(|error| format!("{relative}: {error}"))
}

fn resident_shape_findings(relative: &str, source: &str) -> Result<Vec<String>> {
    let file = parse(relative, source)?;
    let mut findings = Vec::new();
    let Some(root) = named_struct(&file, "ResidentRoot") else {
        return Ok(vec![format!("{relative}: missing ResidentRoot")]);
    };
    let fields = non_test_fields(root);
    if fields.len() != 1
        || fields[0]
            .ident
            .as_ref()
            .is_none_or(|ident| ident != "pages")
        || !type_contains(&fields[0].ty, "BTreeMap")
        || !type_contains(&fields[0].ty, "Arc")
        || !type_contains(&fields[0].ty, "ResidentCell")
    {
        findings.push(format!(
            "{relative}: ResidentRoot must contain only pages: BTreeMap<PageIndex, Arc<ResidentCell>>"
        ));
    }
    for field in fields {
        if type_contains(&field.ty, "SpinMutex")
            || type_contains(&field.ty, "PageSlot")
            || type_contains(&field.ty, "PageService")
            || type_contains(&field.ty, "Block")
            || type_contains(&field.ty, "PageIo")
            || type_contains(&field.ty, "Range")
            || type_contains(&field.ty, "Reservation")
            || type_contains(&field.ty, "Lease")
            || type_contains(&field.ty, "IoData")
        {
            findings.push(format!(
                "{relative}: ResidentRoot embeds mutable authority in field `{}`",
                field_name(field)
            ));
        }
    }

    let Some(cell) = named_struct(&file, "ResidentCell") else {
        return Ok(vec![format!("{relative}: missing ResidentCell")]);
    };
    let cell_fields = non_test_fields(cell);
    let binding_fields = cell_fields
        .iter()
        .filter(|field| type_is_named(&field.ty, "ResidentBindingPin"))
        .collect::<Vec<_>>();
    if binding_fields.len() != 1
        || binding_fields[0]
            .ident
            .as_ref()
            .is_none_or(|ident| ident != "resident_binding")
    {
        findings.push(format!(
            "{relative}: ResidentCell must retain exactly one ResidentBindingPin field"
        ));
    }
    for field in cell_fields {
        if type_contains(&field.ty, "PageCachePin") {
            findings.push(format!(
                "{relative}: ResidentCell must use ResidentBindingPin, not raw PageCachePin (`{}`)",
                field_name(field)
            ));
        }
        if is_dirty_or_writeback_field(field) {
            findings.push(format!(
                "{relative}: ResidentCell must not own dirty/writeback authority (`{}`)",
                field_name(field)
            ));
        }
    }
    Ok(findings)
}

fn page_container_shape_findings(relative: &str, source: &str) -> Result<Vec<String>> {
    let file = parse(relative, source)?;
    let mut findings = Vec::new();

    let Some(container) = named_struct(&file, "PageContainer") else {
        return Ok(vec![format!("{relative}: missing PageContainer")]);
    };
    for (name, required) in [
        ("resident", "Published"),
        ("page_submission", "PageIoSubmissionHandle"),
        ("block_submission", "BlockSubmissionHandle"),
    ] {
        if !non_test_fields(container).iter().any(|field| {
            field.ident.as_ref().is_some_and(|ident| ident == name)
                && type_contains(&field.ty, required)
        }) {
            findings.push(format!(
                "{relative}: PageContainer must retain `{name}: {required}<...>`"
            ));
        }
    }

    let Some(state) = named_struct(&file, "PageContainerState") else {
        return Ok(vec![format!("{relative}: missing PageContainerState")]);
    };
    const EMBEDDED_MANAGER_TYPES: &[&str] = &[
        "PageService",
        "PageIoSubmissionManager",
        "BlockSubmissionManager",
        "BlockSubmissionState",
        "BlockQueue",
        "BlockPageRequestTracker",
        "BlockTagTable",
        "QueueDepth",
    ];
    for field in non_test_fields(state) {
        if let Some(type_name) = EMBEDDED_MANAGER_TYPES
            .iter()
            .find(|type_name| type_contains(&field.ty, type_name))
        {
            findings.push(format!(
                "{relative}: PageContainerState embeds {type_name} in `{}`; L4/L6 state belongs to typed managers",
                field_name(field)
            ));
        }
        if is_dirty_or_writeback_field(field) {
            findings.push(format!(
                "{relative}: PageContainerState must not own dirty/writeback authority (`{}`)",
                field_name(field)
            ));
        }
    }

    if let Some(entry) = named_struct(&file, "PageCacheEntry") {
        let fields = non_test_fields(entry);
        if fields.len() != 2
            || !fields.iter().any(|field| {
                field.ident.as_ref().is_some_and(|ident| ident == "cell")
                    && type_contains(&field.ty, "ResidentCell")
            })
            || !fields.iter().any(|field| {
                field.ident.as_ref().is_some_and(|ident| ident == "marks")
                    && type_is_named(&field.ty, "PageMarks")
            })
        {
            findings.push(format!(
                "{relative}: PageCacheEntry must contain only ResidentCell and non-authoritative PageMarks"
            ));
        }
        for field in fields {
            if is_dirty_or_writeback_field(field) {
                findings.push(format!(
                    "{relative}: PageCacheEntry must not duplicate PageSlot dirty/writeback authority (`{}`)",
                    field_name(field)
                ));
            }
        }
    } else {
        findings.push(format!("{relative}: missing PageCacheEntry"));
    }
    if !source.contains("enum PageCacheMark")
        || source.contains("PageCacheMark::Dirty")
        || source.contains("PageCacheMark::Writeback")
    {
        findings.push(format!(
            "{relative}: PageCacheMark must not regain dirty/writeback authority; PageSlot is sole authority"
        ));
    }
    Ok(findings)
}

fn manager_shape_findings(relative: &str, source: &str, manager: &str) -> Result<Vec<String>> {
    let file = parse(relative, source)?;
    let Some(item) = named_struct(&file, manager) else {
        return Ok(vec![format!("{relative}: missing {manager}")]);
    };
    let fields = non_test_fields(item);
    if fields.len() == 1
        && fields[0]
            .ident
            .as_ref()
            .is_some_and(|ident| ident == "state")
        && type_contains(&fields[0].ty, "SpinMutex")
    {
        Ok(Vec::new())
    } else {
        Ok(vec![format!(
            "{relative}: {manager} must own mutable state behind one SpinMutex field"
        )])
    }
}

fn hit_path_findings(relative: &str, source: &str) -> Vec<String> {
    let mut findings = Vec::new();
    for function in ["lookup_resident_with_guard", "materialize_published_read"] {
        let Some(body) = function_body(source, function) else {
            findings.push(format!(
                "{relative}: missing resident hit function `{function}`"
            ));
            continue;
        };
        for forbidden in [
            ".lock(",
            ".lock_state(",
            ".with_service(",
            "page_submission",
            "block_submission",
        ] {
            if body.contains(forbidden) {
                findings.push(format!(
                    "{relative}: resident hit function `{function}` reaches mutable authority `{forbidden}`"
                ));
            }
        }
    }
    findings
}

fn retirement_findings(relative: &str, source: &str) -> Vec<String> {
    let required = [
        "RESIDENT_ROOT_RETIRE_MAINTENANCE_BUDGET",
        "RESIDENT_ROOT_RETIRE_MAINTENANCE_ATTEMPTS",
        "try_reserve_local_retire",
        "drain_with_budget",
        "commit_reserved",
    ];
    let mut findings = required
        .into_iter()
        .filter(|term| !source.contains(term))
        .map(|term| format!("{relative}: resident-root retirement must retain bounded `{term}`"))
        .collect::<Vec<_>>();
    if source.contains("Vec<ResidentRoot>") || source.contains("Vec < ResidentRoot >") {
        findings.push(format!(
            "{relative}: resident-root retirement must use bounded epoch reservations, not Vec<ResidentRoot>"
        ));
    }
    findings
}

fn guard_bearing_async_state_findings(relative: &str, source: &str) -> Result<Vec<String>> {
    let file = parse(relative, source)?;
    let mut findings = Vec::new();
    for item in &file.items {
        let Item::Struct(item) = item else {
            continue;
        };
        let name = item.ident.to_string();
        if !(name.ends_with("State")
            || name.ends_with("Op")
            || name.ends_with("InFlight")
            || name.ends_with("Submission"))
        {
            continue;
        }
        for field in non_test_fields(item) {
            if type_contains(&field.ty, "Guard") {
                findings.push(format!(
                    "{relative}: async state `{name}` stores an epoch guard in `{}`",
                    field_name(field)
                ));
            }
        }
    }
    Ok(findings)
}

fn named_struct<'a>(file: &'a syn::File, name: &str) -> Option<&'a ItemStruct> {
    file.items.iter().find_map(|item| match item {
        Item::Struct(item) if item.ident == name => Some(item),
        _ => None,
    })
}

fn non_test_fields(item: &ItemStruct) -> Vec<&Field> {
    match &item.fields {
        Fields::Named(fields) => fields
            .named
            .iter()
            .filter(|field| !field.attrs.iter().any(is_test_cfg))
            .collect(),
        Fields::Unnamed(fields) => fields
            .unnamed
            .iter()
            .filter(|field| !field.attrs.iter().any(is_test_cfg))
            .collect(),
        Fields::Unit => Vec::new(),
    }
}

fn is_test_cfg(attribute: &syn::Attribute) -> bool {
    attribute.path().is_ident("cfg")
        && matches!(
            &attribute.meta,
            syn::Meta::List(list) if list.tokens.to_string().contains("test")
        )
}

fn field_name(field: &Field) -> String {
    field
        .ident
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| "<unnamed>".into())
}

fn is_dirty_or_writeback_field(field: &Field) -> bool {
    field.ident.as_ref().is_some_and(|ident| {
        let name = ident.to_string();
        name.contains("dirty") || name.contains("writeback")
    })
}

fn type_is_named(ty: &Type, expected: &str) -> bool {
    matches!(ty, Type::Path(path) if path.path.segments.last().is_some_and(|segment| segment.ident == expected))
}

fn type_contains(ty: &Type, expected: &str) -> bool {
    match ty {
        Type::Path(path) => path.path.segments.iter().any(|segment| {
            segment.ident == expected
                || match &segment.arguments {
                    syn::PathArguments::AngleBracketed(arguments) => {
                        arguments.args.iter().any(|argument| match argument {
                            syn::GenericArgument::Type(ty) => type_contains(ty, expected),
                            syn::GenericArgument::AssocType(assoc) => {
                                type_contains(&assoc.ty, expected)
                            }
                            _ => false,
                        })
                    }
                    _ => false,
                }
        }),
        Type::Reference(reference) => type_contains(&reference.elem, expected),
        Type::Paren(paren) => type_contains(&paren.elem, expected),
        Type::Group(group) => type_contains(&group.elem, expected),
        Type::Ptr(pointer) => type_contains(&pointer.elem, expected),
        Type::Tuple(tuple) => tuple.elems.iter().any(|ty| type_contains(ty, expected)),
        _ => false,
    }
}

fn function_body<'a>(source: &'a str, name: &str) -> Option<&'a str> {
    let anchor = format!("fn {name}");
    let start = source.find(&anchor)?;
    let open = source[start..].find('{')? + start;
    let mut depth = 0usize;
    for (offset, byte) in source[open..].bytes().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(&source[open..=open + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resident_root_rejects_embedded_mutable_authority() {
        let findings = resident_shape_findings(
            "fixture.rs",
            "struct ResidentBindingPin; struct ResidentCell { resident_binding: ResidentBindingPin, dirty: bool } struct ResidentRoot { pages: BTreeMap<PageIndex, Arc<ResidentCell>>, queue: SpinMutex<()> }",
        )
        .expect("fixture parses");
        assert!(findings
            .iter()
            .any(|finding| finding.contains("ResidentRoot")));
        assert!(findings
            .iter()
            .any(|finding| finding.contains("dirty/writeback")));
    }

    #[test]
    fn pagecontainer_state_rejects_embedded_l4_state() {
        let findings = page_container_shape_findings(
            "fixture.rs",
            "struct Published<T>(T); struct ResidentRoot; struct PageIoSubmissionHandle; struct BlockSubmissionHandle; struct PageContainer { resident: Published<ResidentRoot>, page_submission: PageIoSubmissionHandle, block_submission: BlockSubmissionHandle } struct PageContainerState { service: PageService } struct PageCacheEntry { cell: usize } struct PageService;",
        )
        .expect("fixture parses");
        assert!(findings
            .iter()
            .any(|finding| finding.contains("PageService")));
    }

    #[test]
    fn resident_hit_rejects_manager_lock_acquisition() {
        let findings = hit_path_findings(
            "fixture.rs",
            "fn lookup_resident_with_guard() { self.state.lock(); } fn materialize_published_read() {}",
        );
        assert!(findings.iter().any(|finding| finding.contains(".lock(")));
    }

    #[test]
    fn async_state_rejects_stored_guard() {
        let findings = guard_bearing_async_state_findings(
            "fixture.rs",
            "struct FetchState<'g> { guard: Guard<'g> }",
        )
        .expect("fixture parses");
        assert!(findings
            .iter()
            .any(|finding| finding.contains("epoch guard")));
    }

    #[test]
    fn current_pagecontainer_sources_satisfy_ratchet_shapes() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root");
        lint_pagecontainer_resident_rcu(root).expect("current source satisfies ratchet");
    }
}
