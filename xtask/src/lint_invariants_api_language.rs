//! API-language convergence report with hard RCU backend ratchets.
//!
//! This inventories adapter mechanism vocabulary and raw wait/readiness terms
//! while subsystem adapters are still converging on role-shaped exports. Those
//! buckets remain report-only; raw RCU retirement and publication leakage are
//! enforced against explicit ceilings.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use proc_macro2::{Span, TokenStream, TokenTree};
use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use crate::Result;
use crate::util::{collect_files, relative};

const ADAPTER_BUCKET: &str = "adapter mechanism language";
const WAIT_BUCKET: &str = "wait readiness raw language";
const RAW_RCU_BUCKET: &str = "raw rcu backend language";
const PUBLICATION_LEAK_BUCKET: &str = "publication backend leakage";

const MAX_RAW_RETIRE_SITES: usize = 0;
const MAX_RAW_RCU_BACKEND_SITES: usize = 0;
const MAX_PUBLICATION_BACKEND_LEAKS: usize = 0;

const RCU_SCAN_ROOTS: [&str; 20] = [
    "crates/tx-drivers/src",
    "crates/tx-ext4/src",
    "crates/tx-ext4-format/src",
    "crates/tx-fat/src",
    "crates/tx-fat-format/src",
    "crates/tx-fs/src",
    "crates/tx-hal/src",
    "crates/tx-kernel/src",
    "crates/tx-observe/src",
    "crates/tx-observe-types/src",
    "crates/tx-platform-adapter/src",
    "crates/tx-policy/src",
    "crates/tx-reactor/src",
    "crates/tx-scripts/src",
    "crates/tx-services/src",
    "crates/tx-shims/src",
    "crates/tx-subsystems/src",
    "crates/tx-time/src",
    "crates/tx-vdso/src",
    "boards",
];

const STRICT_RCU_ALIAS_ROOTS: [&str; 5] = [
    "crates/tx-subsystems/src",
    "crates/tx-shims/src",
    "crates/tx-scripts/src",
    "crates/tx-reactor/src",
    "crates/tx-kernel/src",
];

const ADAPTER_FORBIDDEN_TERMS: &[&str] = &[
    "ZonePolicy",
    "PayloadPolicy",
    "RetainedEntityPolicy",
    "CapProducingPolicy",
    "CoLocatedEntity",
    "IsPayloadPolicy",
    "ObserverNodePolicy",
    "PayloadBinding",
    "IdentitySlot",
    "OperationalRefExt",
    "RawPort",
    "RawQueue",
    "Channel",
    "Mask",
];

const WAIT_RAW_TERMS: &[&str] = &[
    "WaitToken",
    "WaitSourceId",
    "source_id",
    "InterestMask",
    "Channel",
    "Mask",
    "RawPort",
    "RawQueue",
];

#[derive(Debug, PartialEq, Eq)]
struct Finding {
    rel: String,
    line: usize,
    bucket: &'static str,
    term: &'static str,
    snippet: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RcuModuleKind {
    Substrate,
    Publication,
    Epoch,
}

pub(crate) fn lint_invariants_api_language(root: &Path) -> Result<()> {
    let mut findings = Vec::new();

    for root_rel in RCU_SCAN_ROOTS {
        let path = root.join(root_rel);
        if !path.exists() {
            continue;
        }
        for file in collect_files(&path, &["rs"]).map_err(|err| err.to_string())? {
            let rel = relative(root, &file).replace('\\', "/");
            let text = fs::read_to_string(&file).map_err(|err| format!("{rel}: {err}"))?;
            findings.extend(lint_api_language_text_checked(&rel, &text)?);
        }
    }

    print_api_language_report(&findings);
    enforce_rcu_ratchets(&findings)
}

#[cfg(test)]
fn lint_api_language_text(rel: &str, text: &str) -> Vec<Finding> {
    lint_api_language_text_checked(rel, text).expect("test Rust source must parse")
}

fn lint_api_language_text_checked(rel: &str, text: &str) -> Result<Vec<Finding>> {
    let mut findings = Vec::new();
    let scan_adapter = is_adapter_scan_path(rel);
    let scan_wait = is_wait_raw_scan_path(rel);
    let scan_rcu = is_rcu_scan_path(rel);

    if !scan_adapter && !scan_wait && !scan_rcu {
        return Ok(findings);
    }

    for (line_idx, line) in text.lines().enumerate() {
        let Some(code) = code_before_comment(line) else {
            continue;
        };

        if scan_adapter {
            findings.extend(
                ADAPTER_FORBIDDEN_TERMS
                    .iter()
                    .copied()
                    .filter(|term| contains_ident(code, term))
                    .map(|term| finding(rel, line_idx + 1, ADAPTER_BUCKET, term, code)),
            );
        }

        if scan_wait {
            findings.extend(
                WAIT_RAW_TERMS
                    .iter()
                    .copied()
                    .filter(|term| contains_ident(code, term))
                    .map(|term| finding(rel, line_idx + 1, WAIT_BUCKET, term, code)),
            );
        }
    }

    if scan_rcu {
        findings.extend(rcu_backend_findings(rel, text)?);
    }

    Ok(findings)
}

fn enforce_rcu_ratchets(findings: &[Finding]) -> Result<()> {
    let raw_retire = findings
        .iter()
        .filter(|finding| finding.bucket == RAW_RCU_BUCKET && finding.term == "retire_raw")
        .count();
    let publication_backend = findings
        .iter()
        .filter(|finding| finding.bucket == PUBLICATION_LEAK_BUCKET)
        .count();
    let raw_retire_bypass = findings
        .iter()
        .filter(|finding| {
            finding.bucket == RAW_RCU_BUCKET
                && matches!(finding.term, "retire_raw-import" | "retire_raw-bare")
        })
        .count();
    let raw_backend = findings
        .iter()
        .filter(|finding| {
            finding.bucket == RAW_RCU_BUCKET
                && matches!(
                    finding.term,
                    "AtomicPtr-publication-root" | "RcuHead" | "raw-reclaim-callback"
                )
        })
        .count();

    let mut errors = Vec::new();
    if raw_retire > MAX_RAW_RETIRE_SITES {
        errors.push(format!(
            "raw retire ratchet regression: {raw_retire} > ceiling {MAX_RAW_RETIRE_SITES}"
        ));
    }
    if raw_retire_bypass != 0 {
        errors.push(format!(
            "raw retire import or bare call forbidden: {raw_retire_bypass} > ceiling 0"
        ));
    }
    if raw_backend > MAX_RAW_RCU_BACKEND_SITES {
        errors.push(format!(
            "raw rcu backend ratchet regression: {raw_backend} > ceiling {MAX_RAW_RCU_BACKEND_SITES}"
        ));
    }
    if publication_backend > MAX_PUBLICATION_BACKEND_LEAKS {
        errors.push(format!(
            "publication backend ratchet regression: {publication_backend} > ceiling {MAX_PUBLICATION_BACKEND_LEAKS}"
        ));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

fn print_api_language_report(findings: &[Finding]) {
    let mut by_bucket: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut by_term: BTreeMap<(&'static str, &'static str), usize> = BTreeMap::new();
    for finding in findings {
        *by_bucket.entry(finding.bucket).or_insert(0) += 1;
        *by_term.entry((finding.bucket, finding.term)).or_insert(0) += 1;
    }

    println!("Invariants Lint - api-language");
    println!("===============================");
    println!(
        "txdoc limited-language/api-language findings: {:>4}  (adapter/wait report-only; RCU ratcheted)",
        findings.len()
    );

    if !by_bucket.is_empty() {
        println!();
        println!("  by bucket:");
        for (bucket, count) in &by_bucket {
            println!("  {bucket:<36} {count:>4}");
        }
    }

    if !by_term.is_empty() {
        println!();
        println!("  by term:");
        for ((bucket, term), count) in &by_term {
            println!("  {bucket:<36} {term:<24} {count:>4}");
        }
    }

    if !findings.is_empty() {
        println!();
        println!("  sites (first 120):");
        let ordered = findings
            .iter()
            .filter(|finding| matches!(finding.bucket, RAW_RCU_BUCKET | PUBLICATION_LEAK_BUCKET))
            .chain(findings.iter().filter(|finding| {
                !matches!(finding.bucket, RAW_RCU_BUCKET | PUBLICATION_LEAK_BUCKET)
            }));
        for finding in ordered.take(120) {
            println!(
                "  {}:{} - {} `{}`: {}",
                finding.rel, finding.line, finding.bucket, finding.term, finding.snippet
            );
        }
        if findings.len() > 120 {
            println!("  ... and {} more", findings.len() - 120);
        }
    }
}

fn finding(
    rel: &str,
    line: usize,
    bucket: &'static str,
    term: &'static str,
    code: &str,
) -> Finding {
    Finding {
        rel: rel.to_string(),
        line,
        bucket,
        term,
        snippet: code.trim().chars().take(140).collect(),
    }
}

fn is_adapter_scan_path(rel: &str) -> bool {
    rel == "crates/tx-shims/src/adapter.rs"
        || rel == "crates/tx-subsystems/src/adapter.rs"
        || (rel.starts_with("crates/tx-subsystems/src/") && rel.ends_with("/adapter.rs"))
}

fn is_wait_raw_scan_path(rel: &str) -> bool {
    (rel.starts_with("crates/tx-subsystems/src/") || rel.starts_with("crates/tx-shims/src/"))
        && rel.ends_with(".rs")
        && !rel.ends_with("/notification.rs")
        && !rel.ends_with("/wait_source.rs")
        && !rel.ends_with("/adapter.rs")
        && rel != "crates/tx-shims/src/adapter.rs"
        && !rel.contains("/substrate/")
        && !rel.contains("/reactor/")
}

fn is_rcu_scan_path(rel: &str) -> bool {
    RCU_SCAN_ROOTS
        .iter()
        .any(|root| rel == *root || rel.starts_with(&format!("{root}/")))
}

fn rcu_backend_findings(rel: &str, text: &str) -> Result<Vec<Finding>> {
    let file = syn::parse_file(text).map_err(|err| format!("{rel}: Rust parse failed: {err}"))?;
    let publication_aliases = publication_aliases(&file);
    let (module_aliases, mut unresolved_module_aliases) = rcu_module_aliases(&file);
    let scan_unresolved_aliases = STRICT_RCU_ALIAS_ROOTS
        .iter()
        .any(|root| rel == *root || rel.starts_with(&format!("{root}/")));
    if !scan_unresolved_aliases {
        unresolved_module_aliases.clear();
    }
    let public_glob_macros = public_glob_macros(&file);
    let private_traits = private_trait_names(&file);
    let mut visitor = RcuBackendVisitor {
        rel,
        lines: text.lines().collect(),
        publication_aliases,
        module_aliases,
        unresolved_module_aliases,
        public_glob_macros,
        private_traits,
        scan_unresolved_aliases,
        module_path: Vec::new(),
        findings: Vec::new(),
    };
    visitor.visit_file(&file);
    Ok(visitor.findings)
}

struct RcuBackendVisitor<'source> {
    rel: &'source str,
    lines: Vec<&'source str>,
    publication_aliases: BTreeMap<String, &'static str>,
    module_aliases: BTreeMap<(Vec<String>, String), RcuModuleKind>,
    unresolved_module_aliases: BTreeSet<(Vec<String>, String)>,
    public_glob_macros: BTreeSet<String>,
    private_traits: BTreeSet<(Vec<String>, String)>,
    scan_unresolved_aliases: bool,
    module_path: Vec<String>,
    findings: Vec<Finding>,
}

impl RcuBackendVisitor<'_> {
    fn module_alias_kind(&self, ident: &syn::Ident) -> Option<RcuModuleKind> {
        lookup_module_alias(&self.module_aliases, &self.module_path, &ident.to_string())
    }

    fn unresolved_module_alias(&self, ident: &syn::Ident) -> bool {
        lookup_scoped_name(
            &self.unresolved_module_aliases,
            &self.module_path,
            &ident.to_string(),
        )
    }

    fn push(&mut self, span: Span, bucket: &'static str, term: &'static str) {
        let line = span.start().line;
        let snippet = self
            .lines
            .get(line.saturating_sub(1))
            .copied()
            .unwrap_or("");
        self.findings
            .push(finding(self.rel, line, bucket, term, snippet));
    }

    fn push_publication_ident(&mut self, ident: &syn::Ident) {
        if let Some(term) = publication_term(ident, &self.publication_aliases) {
            self.push(ident.span(), PUBLICATION_LEAK_BUCKET, term);
        }
    }

    fn push_publication_leaks(&mut self, leaks: Vec<(Span, &'static str)>) {
        for (span, term) in leaks {
            self.push(span, PUBLICATION_LEAK_BUCKET, term);
        }
    }

    fn push_publication_module(&mut self, span: Span) {
        for term in ["Published", "PublishReservation", "PublishError"] {
            self.push(span, PUBLICATION_LEAK_BUCKET, term);
        }
    }

    fn push_epoch_module(&mut self, span: Span) {
        self.push(span, RAW_RCU_BUCKET, "retire_raw-import");
    }

    fn push_substrate_module(&mut self, span: Span) {
        self.push_publication_module(span);
        self.push_epoch_module(span);
    }

    fn push_module_kind(&mut self, kind: RcuModuleKind, span: Span) {
        match kind {
            RcuModuleKind::Substrate => self.push_substrate_module(span),
            RcuModuleKind::Publication => self.push_publication_module(span),
            RcuModuleKind::Epoch => self.push_epoch_module(span),
        }
    }

    fn scan_signature(&mut self, signature: &syn::Signature) {
        let leaks = publication_leaks(&self.publication_aliases, |visitor| {
            visitor.visit_signature(signature);
        });
        self.push_publication_leaks(leaks);
    }

    fn scan_generics(&mut self, generics: &syn::Generics) {
        let leaks = publication_leaks(&self.publication_aliases, |visitor| {
            visitor.visit_generics(generics);
        });
        self.push_publication_leaks(leaks);
    }

    fn scan_type(&mut self, ty: &syn::Type) {
        let leaks = publication_leaks(&self.publication_aliases, |visitor| {
            visitor.visit_type(ty);
        });
        self.push_publication_leaks(leaks);
    }

    fn scan_path(&mut self, path: &syn::Path) {
        let leaks = publication_leaks(&self.publication_aliases, |visitor| {
            visitor.visit_path(path);
        });
        self.push_publication_leaks(leaks);
    }

    fn scan_type_param_bound(&mut self, bound: &syn::TypeParamBound) {
        let leaks = publication_leaks(&self.publication_aliases, |visitor| {
            visitor.visit_type_param_bound(bound);
        });
        self.push_publication_leaks(leaks);
    }

    fn scan_public_use_tree(
        &mut self,
        tree: &syn::UseTree,
        substrate_path: bool,
        publication_path: bool,
        epoch_path: bool,
    ) {
        match tree {
            syn::UseTree::Name(name) if self.module_alias_kind(&name.ident).is_some() => {
                self.push_module_kind(
                    self.module_alias_kind(&name.ident).expect("guarded alias"),
                    name.ident.span(),
                );
            }
            syn::UseTree::Name(name)
                if (substrate_path && name.ident == "publication")
                    || (publication_path && name.ident == "self") =>
            {
                self.push_publication_module(name.ident.span());
            }
            syn::UseTree::Name(name)
                if (substrate_path && name.ident == "epoch")
                    || (epoch_path && name.ident == "self") =>
            {
                self.push_epoch_module(name.ident.span());
            }
            syn::UseTree::Name(name)
                if name.ident == "tx_substrate"
                    || (substrate_path
                        && !publication_path
                        && !epoch_path
                        && name.ident == "self") =>
            {
                self.push_substrate_module(name.ident.span());
            }
            syn::UseTree::Name(name) => self.push_publication_ident(&name.ident),
            syn::UseTree::Rename(rename) if self.module_alias_kind(&rename.ident).is_some() => {
                self.push_module_kind(
                    self.module_alias_kind(&rename.ident)
                        .expect("guarded alias"),
                    rename.ident.span(),
                );
            }
            syn::UseTree::Rename(rename)
                if (substrate_path && rename.ident == "publication")
                    || (publication_path && rename.ident == "self") =>
            {
                self.push_publication_module(rename.ident.span());
            }
            syn::UseTree::Rename(rename)
                if (substrate_path && rename.ident == "epoch")
                    || (epoch_path && rename.ident == "self") =>
            {
                self.push_epoch_module(rename.ident.span());
            }
            syn::UseTree::Rename(rename)
                if rename.ident == "tx_substrate"
                    || (substrate_path
                        && !publication_path
                        && !epoch_path
                        && rename.ident == "self") =>
            {
                self.push_substrate_module(rename.ident.span());
            }
            syn::UseTree::Rename(rename) => self.push_publication_ident(&rename.ident),
            syn::UseTree::Path(path) => {
                self.push_publication_ident(&path.ident);
                let alias_kind = self.module_alias_kind(&path.ident);
                let next_substrate =
                    path.ident == "tx_substrate" || alias_kind == Some(RcuModuleKind::Substrate);
                self.scan_public_use_tree(
                    &path.tree,
                    next_substrate,
                    publication_path
                        || (substrate_path && path.ident == "publication")
                        || alias_kind == Some(RcuModuleKind::Publication),
                    epoch_path
                        || (substrate_path && path.ident == "epoch")
                        || alias_kind == Some(RcuModuleKind::Epoch),
                );
            }
            syn::UseTree::Group(group) => {
                for item in &group.items {
                    self.scan_public_use_tree(item, substrate_path, publication_path, epoch_path);
                }
            }
            syn::UseTree::Glob(glob) if publication_path => {
                self.push_publication_module(glob.star_token.span());
            }
            syn::UseTree::Glob(glob) if epoch_path => {
                self.push_epoch_module(glob.star_token.span());
            }
            syn::UseTree::Glob(glob) if substrate_path => {
                self.push_substrate_module(glob.star_token.span());
            }
            syn::UseTree::Glob(_) => {}
        }
    }

    fn scan_unresolved_relative_globs(&mut self, tree: &syn::UseTree) {
        let mut globs = Vec::new();
        collect_glob_use_paths(tree, &mut Vec::new(), &mut globs);
        for (path, span) in globs {
            let mut relative_end = 0usize;
            match path.first().map(String::as_str) {
                Some("crate" | "self") => relative_end = 1,
                Some("super") => {
                    while path
                        .get(relative_end)
                        .is_some_and(|segment| segment == "super")
                    {
                        relative_end += 1;
                    }
                }
                _ => {}
            }
            let unresolved_local = path.len() == 1
                && lookup_scoped_name(&self.unresolved_module_aliases, &self.module_path, &path[0]);
            if (relative_end != 0
                && path.len() == relative_end + 1
                && classify_module_target(&path, &self.module_path, &self.module_aliases).is_none())
                || unresolved_local
            {
                self.push_substrate_module(span);
            }
        }
    }

    fn scan_macro_tokens(
        &mut self,
        tokens: TokenStream,
        scan_publication: bool,
        scan_unresolved_alias: bool,
    ) {
        let tokens: Vec<_> = tokens.into_iter().collect();
        self.scan_macro_module_paths(&tokens, scan_unresolved_alias);
        for (index, token) in tokens.iter().enumerate() {
            match token {
                TokenTree::Group(group) => {
                    self.scan_macro_tokens(group.stream(), scan_publication, scan_unresolved_alias)
                }
                TokenTree::Ident(ident) if ident == "retire_raw" => {
                    self.push(ident.span(), RAW_RCU_BUCKET, "retire_raw-bare");
                }
                TokenTree::Ident(ident) => {
                    let generic_type = tokens.get(index + 1).is_some_and(
                        |next| matches!(next, TokenTree::Punct(punct) if punct.as_char() == '<'),
                    );
                    let qualified = index >= 2
                        && tokens[index - 2..index].iter().all(
                            |token| matches!(token, TokenTree::Punct(punct) if punct.as_char() == ':'),
                        );
                    let backend_qualified = qualified
                        && index >= 3
                        && matches!(
                            &tokens[index - 3],
                            TokenTree::Ident(qualifier)
                                if qualifier == "publication"
                                    || self.publication_aliases.contains_key(&qualifier.to_string())
                        );
                    let known_alias = self.publication_aliases.contains_key(&ident.to_string());
                    if scan_publication
                        || generic_type
                        || known_alias
                        || publication_term(ident, &self.publication_aliases)
                            .is_some_and(|term| term != "Published")
                        || (ident == "Published" && (!qualified || backend_qualified))
                    {
                        self.push_publication_ident(ident);
                    }
                }
                TokenTree::Punct(_) | TokenTree::Literal(_) => {}
            }
        }
    }

    fn scan_macro_module_paths(&mut self, tokens: &[TokenTree], scan_unresolved_alias: bool) {
        for (start, token) in tokens.iter().enumerate() {
            let TokenTree::Ident(root) = token else {
                continue;
            };

            let mut segments = vec![root];
            let mut cursor = start + 1;
            while cursor + 2 < tokens.len()
                && matches!(&tokens[cursor], TokenTree::Punct(punct) if punct.as_char() == ':')
                && matches!(&tokens[cursor + 1], TokenTree::Punct(punct) if punct.as_char() == ':')
            {
                let TokenTree::Ident(segment) = &tokens[cursor + 2] else {
                    break;
                };
                segments.push(segment);
                cursor += 3;
            }

            let glob_tail = cursor + 2 < tokens.len()
                && matches!(&tokens[cursor], TokenTree::Punct(punct) if punct.as_char() == ':')
                && matches!(&tokens[cursor + 1], TokenTree::Punct(punct) if punct.as_char() == ':')
                && matches!(&tokens[cursor + 2], TokenTree::Punct(punct) if punct.as_char() == '*');
            let path_ended = cursor == tokens.len()
                || tokens.get(cursor).is_some_and(|tail| {
                    matches!(tail, TokenTree::Punct(punct) if matches!(punct.as_char(), ',' | ';'))
                        || matches!(tail, TokenTree::Ident(ident) if ident == "as")
                });

            let mut relative_end = 0usize;
            match segments.first().map(|segment| segment.to_string()) {
                Some(relative) if matches!(relative.as_str(), "crate" | "self") => {
                    relative_end = 1;
                }
                Some(relative) if relative == "super" => {
                    while segments
                        .get(relative_end)
                        .is_some_and(|segment| *segment == "super")
                    {
                        relative_end += 1;
                    }
                }
                _ => {}
            }
            if scan_unresolved_alias
                && relative_end != 0
                && segments.len() == relative_end + 1
                && (path_ended || glob_tail)
            {
                self.push_substrate_module(segments[relative_end].span());
                continue;
            }

            let root_kind = if root == "tx_substrate" {
                Some(RcuModuleKind::Substrate)
            } else if scan_unresolved_alias && self.unresolved_module_alias(root) {
                Some(RcuModuleKind::Substrate)
            } else {
                self.module_alias_kind(root)
            };
            let Some(root_kind) = root_kind else {
                continue;
            };

            match root_kind {
                RcuModuleKind::Publication => self.push_publication_module(root.span()),
                RcuModuleKind::Epoch if segments.len() == 1 && (path_ended || glob_tail) => {
                    self.push_epoch_module(root.span());
                }
                RcuModuleKind::Substrate
                    if segments
                        .get(1)
                        .is_some_and(|segment| *segment == "publication") =>
                {
                    self.push_publication_module(segments[1].span());
                }
                RcuModuleKind::Substrate
                    if segments.get(1).is_some_and(|segment| *segment == "epoch")
                        && segments.len() == 2
                        && (path_ended || glob_tail) =>
                {
                    self.push_epoch_module(segments[1].span());
                }
                RcuModuleKind::Substrate if segments.len() == 1 && (path_ended || glob_tail) => {
                    self.push_substrate_module(root.span());
                }
                RcuModuleKind::Epoch | RcuModuleKind::Substrate => {}
            }
        }
    }
}

impl<'ast> Visit<'ast> for RcuBackendVisitor<'_> {
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if node.content.is_some() {
            self.module_path.push(node.ident.to_string());
            visit::visit_item_mod(self, node);
            self.module_path.pop();
        } else {
            visit::visit_item_mod(self, node);
        }
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        if node.sig.ident == "retire_raw" {
            self.push(node.sig.ident.span(), RAW_RCU_BUCKET, "retire_raw-bare");
        }
        if raw_reclaim_callback(&node.sig) {
            self.push(
                node.sig.ident.span(),
                RAW_RCU_BUCKET,
                "raw-reclaim-callback",
            );
        }
        if visible(&node.vis) {
            self.scan_signature(&node.sig);
        }
        visit::visit_item_fn(self, node);
    }

    fn visit_item_use(&mut self, node: &'ast syn::ItemUse) {
        if visible(&node.vis) {
            if self.scan_unresolved_aliases {
                self.scan_unresolved_relative_globs(&node.tree);
            }
            self.scan_public_use_tree(&node.tree, false, false, false);
        }
        visit::visit_item_use(self, node);
    }

    fn visit_item_extern_crate(&mut self, node: &'ast syn::ItemExternCrate) {
        if visible(&node.vis) && node.ident == "tx_substrate" {
            self.push_substrate_module(node.ident.span());
        }
        visit::visit_item_extern_crate(self, node);
    }

    fn visit_use_tree(&mut self, tree: &'ast syn::UseTree) {
        match tree {
            syn::UseTree::Name(name) => {
                if name.ident == "retire_raw" {
                    self.push(name.ident.span(), RAW_RCU_BUCKET, "retire_raw-import");
                }
            }
            syn::UseTree::Rename(rename) => {
                if rename.ident == "retire_raw" {
                    self.push(rename.ident.span(), RAW_RCU_BUCKET, "retire_raw-import");
                }
            }
            syn::UseTree::Path(path) => {
                if path.ident == "retire_raw" {
                    self.push(path.ident.span(), RAW_RCU_BUCKET, "retire_raw-import");
                }
            }
            syn::UseTree::Glob(_) | syn::UseTree::Group(_) => {}
        }
        visit::visit_use_tree(self, tree);
    }

    fn visit_item_struct(&mut self, node: &'ast syn::ItemStruct) {
        for field in &node.fields {
            if field.ident.as_ref().is_some_and(publication_root_name)
                && type_mentions(&field.ty, "AtomicPtr")
            {
                self.push(
                    field.ident.as_ref().expect("checked Some").span(),
                    RAW_RCU_BUCKET,
                    "AtomicPtr-publication-root",
                );
            }
        }
        if visible(&node.vis) {
            self.scan_generics(&node.generics);
            for field in &node.fields {
                if visible(&field.vis) {
                    self.scan_type(&field.ty);
                }
            }
        }
        visit::visit_item_struct(self, node);
    }

    fn visit_item_union(&mut self, node: &'ast syn::ItemUnion) {
        if visible(&node.vis) {
            self.scan_generics(&node.generics);
            for field in &node.fields.named {
                if visible(&field.vis) {
                    self.scan_type(&field.ty);
                }
            }
        }
        visit::visit_item_union(self, node);
    }

    fn visit_item_enum(&mut self, node: &'ast syn::ItemEnum) {
        if visible(&node.vis) {
            self.scan_generics(&node.generics);
            for variant in &node.variants {
                for field in &variant.fields {
                    self.scan_type(&field.ty);
                }
            }
        }
        visit::visit_item_enum(self, node);
    }

    fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
        if visible(&node.vis) {
            self.scan_generics(&node.generics);
            for bound in &node.supertraits {
                self.scan_type_param_bound(bound);
            }
            for item in &node.items {
                match item {
                    syn::TraitItem::Fn(function) => self.scan_signature(&function.sig),
                    syn::TraitItem::Const(item_const) => self.scan_type(&item_const.ty),
                    syn::TraitItem::Type(item_type) => {
                        self.scan_generics(&item_type.generics);
                        for bound in &item_type.bounds {
                            self.scan_type_param_bound(bound);
                        }
                        if let Some((_, ty)) = &item_type.default {
                            self.scan_type(ty);
                        }
                    }
                    syn::TraitItem::Macro(_) | syn::TraitItem::Verbatim(_) => {}
                    _ => {}
                }
            }
        }
        visit::visit_item_trait(self, node);
    }

    fn visit_item_trait_alias(&mut self, node: &'ast syn::ItemTraitAlias) {
        if visible(&node.vis) {
            self.scan_generics(&node.generics);
            for bound in &node.bounds {
                self.scan_type_param_bound(bound);
            }
        }
        visit::visit_item_trait_alias(self, node);
    }

    fn visit_item_type(&mut self, node: &'ast syn::ItemType) {
        if visible(&node.vis) {
            self.scan_generics(&node.generics);
            self.scan_type(&node.ty);
        }
        visit::visit_item_type(self, node);
    }

    fn visit_item_const(&mut self, node: &'ast syn::ItemConst) {
        if visible(&node.vis) {
            self.scan_generics(&node.generics);
            self.scan_type(&node.ty);
        }
        visit::visit_item_const(self, node);
    }

    fn visit_item_static(&mut self, node: &'ast syn::ItemStatic) {
        if publication_root_name(&node.ident) && type_mentions(&node.ty, "AtomicPtr") {
            self.push(
                node.ident.span(),
                RAW_RCU_BUCKET,
                "AtomicPtr-publication-root",
            );
        }
        if visible(&node.vis) {
            self.scan_type(&node.ty);
        }
        visit::visit_item_static(self, node);
    }

    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if let Some(path) = raw_callee_path(&node.func) {
            if let Some(term) = retire_raw_path_term(path) {
                self.push(node.span(), RAW_RCU_BUCKET, term);
                for attribute in &node.attrs {
                    self.visit_attribute(attribute);
                }
                for argument in &node.args {
                    self.visit_expr(argument);
                }
                return;
            }
        }
        visit::visit_expr_call(self, node);
    }

    fn visit_expr_path(&mut self, node: &'ast syn::ExprPath) {
        if retire_raw_path_term(&node.path).is_some() {
            self.push(node.span(), RAW_RCU_BUCKET, "retire_raw-bare");
        }
        visit::visit_expr_path(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        if node.method == "retire_raw" {
            self.push(node.method.span(), RAW_RCU_BUCKET, "retire_raw-bare");
        }
        visit::visit_expr_method_call(self, node);
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        if node.sig.ident == "retire_raw" {
            self.push(node.sig.ident.span(), RAW_RCU_BUCKET, "retire_raw-bare");
        }
        if raw_reclaim_callback(&node.sig) {
            self.push(
                node.sig.ident.span(),
                RAW_RCU_BUCKET,
                "raw-reclaim-callback",
            );
        }
        if visible(&node.vis) {
            self.scan_signature(&node.sig);
        }
        visit::visit_impl_item_fn(self, node);
    }

    fn visit_type_path(&mut self, node: &'ast syn::TypePath) {
        if node
            .path
            .segments
            .iter()
            .any(|segment| segment.ident == "RcuHead")
        {
            self.push(node.span(), RAW_RCU_BUCKET, "RcuHead");
        }
        visit::visit_type_path(self, node);
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        let visible_trait_impl = node.trait_.as_ref().is_some_and(|(_, path, _)| {
            path.segments.len() != 1
                || path.segments.last().is_some_and(|segment| {
                    !self
                        .private_traits
                        .contains(&(self.module_path.clone(), segment.ident.to_string()))
                })
        });
        if visible_trait_impl {
            if let Some((_, trait_path, _)) = &node.trait_ {
                self.scan_path(trait_path);
            }
            self.scan_type(&node.self_ty);
            for item in &node.items {
                match item {
                    syn::ImplItem::Fn(function) => self.scan_signature(&function.sig),
                    syn::ImplItem::Const(item_const) => self.scan_type(&item_const.ty),
                    syn::ImplItem::Type(item_type) => {
                        self.scan_generics(&item_type.generics);
                        self.scan_type(&item_type.ty);
                    }
                    syn::ImplItem::Macro(_) | syn::ImplItem::Verbatim(_) => {}
                    _ => {}
                }
            }
        } else if node.trait_.is_none() {
            for item in &node.items {
                match item {
                    syn::ImplItem::Const(item_const) if visible(&item_const.vis) => {
                        self.scan_type(&item_const.ty);
                    }
                    syn::ImplItem::Type(item_type) if visible(&item_type.vis) => {
                        self.scan_generics(&item_type.generics);
                        self.scan_type(&item_type.ty);
                    }
                    _ => {}
                }
            }
        }
        visit::visit_item_impl(self, node);
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        if node.sig.ident == "retire_raw" {
            self.push(node.sig.ident.span(), RAW_RCU_BUCKET, "retire_raw-bare");
        }
        visit::visit_trait_item_fn(self, node);
    }

    fn visit_foreign_item_fn(&mut self, node: &'ast syn::ForeignItemFn) {
        if node.sig.ident == "retire_raw" {
            self.push(node.sig.ident.span(), RAW_RCU_BUCKET, "retire_raw-bare");
        }
        if visible(&node.vis) {
            self.scan_signature(&node.sig);
        }
        visit::visit_foreign_item_fn(self, node);
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        // Macro expansion is unavailable here. Raw retirement is forbidden in
        // all tokens; publication vocabulary is checked in macro definitions,
        // where it could generate an API surface invisible to the AST.
        let may_export_glob =
            node.path.segments.last().is_some_and(|segment| {
                self.public_glob_macros.contains(&segment.ident.to_string())
            });
        self.scan_macro_tokens(
            node.tokens.clone(),
            node.path.is_ident("macro_rules"),
            may_export_glob,
        );
        visit::visit_macro(self, node);
    }
}

fn raw_callee_path(expression: &syn::Expr) -> Option<&syn::Path> {
    match expression {
        syn::Expr::Path(path) => Some(&path.path),
        syn::Expr::Group(group) => raw_callee_path(&group.expr),
        syn::Expr::Paren(paren) => raw_callee_path(&paren.expr),
        _ => None,
    }
}

fn retire_raw_path_term(path: &syn::Path) -> Option<&'static str> {
    let segments: Vec<_> = path.segments.iter().collect();
    if !segments
        .last()
        .is_some_and(|segment| segment.ident == "retire_raw")
    {
        return None;
    }
    if segments
        .iter()
        .rev()
        .nth(1)
        .is_some_and(|segment| segment.ident == "epoch")
    {
        Some("retire_raw")
    } else {
        Some("retire_raw-bare")
    }
}

fn visible(visibility: &syn::Visibility) -> bool {
    !matches!(visibility, syn::Visibility::Inherited)
}

fn publication_root_name(ident: &syn::Ident) -> bool {
    let name = ident.to_string().to_ascii_lowercase();
    ["root", "current", "snapshot", "published"]
        .iter()
        .any(|term| name.contains(term))
}

fn type_mentions(ty: &syn::Type, target: &str) -> bool {
    match ty {
        syn::Type::Array(array) => type_mentions(&array.elem, target),
        syn::Type::Group(group) => type_mentions(&group.elem, target),
        syn::Type::Paren(paren) => type_mentions(&paren.elem, target),
        syn::Type::Path(path) => {
            path.path
                .segments
                .iter()
                .any(|segment| segment.ident == target)
                || path
                    .path
                    .segments
                    .iter()
                    .any(|segment| match &segment.arguments {
                        syn::PathArguments::AngleBracketed(arguments) => {
                            arguments.args.iter().any(|argument| match argument {
                                syn::GenericArgument::Type(ty) => type_mentions(ty, target),
                                _ => false,
                            })
                        }
                        syn::PathArguments::Parenthesized(arguments) => {
                            arguments.inputs.iter().any(|ty| type_mentions(ty, target))
                                || match &arguments.output {
                                    syn::ReturnType::Default => false,
                                    syn::ReturnType::Type(_, ty) => type_mentions(ty, target),
                                }
                        }
                        syn::PathArguments::None => false,
                    })
        }
        syn::Type::Ptr(pointer) => type_mentions(&pointer.elem, target),
        syn::Type::Reference(reference) => type_mentions(&reference.elem, target),
        syn::Type::Slice(slice) => type_mentions(&slice.elem, target),
        syn::Type::Tuple(tuple) => tuple.elems.iter().any(|ty| type_mentions(ty, target)),
        _ => false,
    }
}

fn raw_reclaim_callback(sig: &syn::Signature) -> bool {
    let name = sig.ident.to_string().to_ascii_lowercase();
    if !["reclaim", "retire", "defer"]
        .iter()
        .any(|term| name.contains(term))
    {
        return false;
    }
    sig.inputs.iter().any(|input| match input {
        syn::FnArg::Receiver(_) => false,
        syn::FnArg::Typed(argument) => matches!(argument.ty.as_ref(), syn::Type::Ptr(_)),
    })
}

fn direct_publication_term(ident: &syn::Ident) -> Option<&'static str> {
    if ident == "Published" {
        Some("Published")
    } else if ident == "PublishReservation" {
        Some("PublishReservation")
    } else if ident == "PublishError" {
        Some("PublishError")
    } else {
        None
    }
}

fn publication_term(
    ident: &syn::Ident,
    aliases: &BTreeMap<String, &'static str>,
) -> Option<&'static str> {
    direct_publication_term(ident).or_else(|| aliases.get(&ident.to_string()).copied())
}

struct PublicationAliasCollector<'ast> {
    rename_edges: Vec<(String, String)>,
    type_aliases: Vec<(String, &'ast syn::Type)>,
}

impl<'ast> Visit<'ast> for PublicationAliasCollector<'ast> {
    fn visit_use_tree(&mut self, tree: &'ast syn::UseTree) {
        if let syn::UseTree::Rename(rename) = tree {
            self.rename_edges
                .push((rename.rename.to_string(), rename.ident.to_string()));
        }
        visit::visit_use_tree(self, tree);
    }

    fn visit_item_type(&mut self, node: &'ast syn::ItemType) {
        self.type_aliases
            .push((node.ident.to_string(), node.ty.as_ref()));
        visit::visit_item_type(self, node);
    }
}

fn publication_aliases(file: &syn::File) -> BTreeMap<String, &'static str> {
    let mut collector = PublicationAliasCollector {
        rename_edges: Vec::new(),
        type_aliases: Vec::new(),
    };
    collector.visit_file(file);

    let mut aliases = BTreeMap::new();
    loop {
        let mut changed = false;
        for (alias, original) in &collector.rename_edges {
            let term = match original.as_str() {
                "Published" => Some("Published"),
                "PublishReservation" => Some("PublishReservation"),
                "PublishError" => Some("PublishError"),
                "publication" => Some("Published"),
                _ => aliases.get(original).copied(),
            };
            if let Some(term) = term.filter(|_| !aliases.contains_key(alias)) {
                aliases.insert(alias.clone(), term);
                changed = true;
            }
        }
        for (alias, ty) in &collector.type_aliases {
            let term = publication_leaks(&aliases, |visitor| visitor.visit_type(ty))
                .first()
                .map(|(_, term)| *term);
            if let Some(term) = term.filter(|_| !aliases.contains_key(alias)) {
                aliases.insert(alias.clone(), term);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    aliases
}

struct ModuleAliasCollector {
    module_path: Vec<String>,
    bindings: Vec<(Vec<String>, String, Vec<String>)>,
}

impl<'ast> Visit<'ast> for ModuleAliasCollector {
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if node.content.is_some() {
            self.module_path.push(node.ident.to_string());
            visit::visit_item_mod(self, node);
            self.module_path.pop();
        } else {
            visit::visit_item_mod(self, node);
        }
    }

    fn visit_item_use(&mut self, node: &'ast syn::ItemUse) {
        collect_use_bindings(
            &node.tree,
            &mut Vec::new(),
            &self.module_path,
            &mut self.bindings,
        );
        visit::visit_item_use(self, node);
    }

    fn visit_item_extern_crate(&mut self, node: &'ast syn::ItemExternCrate) {
        if node.ident == "tx_substrate" {
            let local = node
                .rename
                .as_ref()
                .map_or_else(|| node.ident.to_string(), |(_, alias)| alias.to_string());
            self.bindings.push((
                self.module_path.clone(),
                local,
                vec![node.ident.to_string()],
            ));
        }
        visit::visit_item_extern_crate(self, node);
    }
}

fn collect_use_bindings(
    tree: &syn::UseTree,
    prefix: &mut Vec<String>,
    module_path: &[String],
    bindings: &mut Vec<(Vec<String>, String, Vec<String>)>,
) {
    match tree {
        syn::UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            collect_use_bindings(&path.tree, prefix, module_path, bindings);
            prefix.pop();
        }
        syn::UseTree::Name(name) => {
            let mut target = prefix.clone();
            if name.ident != "self" {
                target.push(name.ident.to_string());
            }
            let local = if name.ident == "self" {
                prefix.last().cloned()
            } else {
                Some(name.ident.to_string())
            };
            if let Some(local) = local {
                bindings.push((module_path.to_vec(), local, target));
            }
        }
        syn::UseTree::Rename(rename) => {
            let mut target = prefix.clone();
            if rename.ident != "self" {
                target.push(rename.ident.to_string());
            }
            bindings.push((module_path.to_vec(), rename.rename.to_string(), target));
        }
        syn::UseTree::Group(group) => {
            for item in &group.items {
                collect_use_bindings(item, prefix, module_path, bindings);
            }
        }
        syn::UseTree::Glob(_) => {}
    }
}

fn collect_glob_use_paths(
    tree: &syn::UseTree,
    prefix: &mut Vec<String>,
    globs: &mut Vec<(Vec<String>, Span)>,
) {
    match tree {
        syn::UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            collect_glob_use_paths(&path.tree, prefix, globs);
            prefix.pop();
        }
        syn::UseTree::Group(group) => {
            for item in &group.items {
                collect_glob_use_paths(item, prefix, globs);
            }
        }
        syn::UseTree::Glob(glob) => globs.push((prefix.clone(), glob.star_token.span())),
        syn::UseTree::Name(_) | syn::UseTree::Rename(_) => {}
    }
}

fn lookup_module_alias(
    aliases: &BTreeMap<(Vec<String>, String), RcuModuleKind>,
    module_path: &[String],
    name: &str,
) -> Option<RcuModuleKind> {
    (0..=module_path.len()).rev().find_map(|depth| {
        aliases
            .get(&(module_path[..depth].to_vec(), name.to_string()))
            .copied()
    })
}

fn lookup_scoped_name(
    names: &BTreeSet<(Vec<String>, String)>,
    module_path: &[String],
    name: &str,
) -> bool {
    (0..=module_path.len())
        .rev()
        .any(|depth| names.contains(&(module_path[..depth].to_vec(), name.to_string())))
}

fn classify_module_target(
    target: &[String],
    module_path: &[String],
    aliases: &BTreeMap<(Vec<String>, String), RcuModuleKind>,
) -> Option<RcuModuleKind> {
    let mut lookup_path = module_path.to_vec();
    let mut start = 0usize;
    match target.first().map(String::as_str) {
        Some("crate") => {
            lookup_path.clear();
            start = 1;
        }
        Some("self") => start = 1,
        Some("super") => {
            while target.get(start).is_some_and(|segment| segment == "super") {
                lookup_path.pop()?;
                start += 1;
            }
        }
        _ => {}
    }

    let (first, rest) = target.get(start..)?.split_first()?;
    let mut kind = if first == "tx_substrate" {
        RcuModuleKind::Substrate
    } else {
        lookup_module_alias(aliases, &lookup_path, first)?
    };

    for segment in rest {
        kind = match (kind, segment.as_str()) {
            (RcuModuleKind::Substrate, "publication") => RcuModuleKind::Publication,
            (RcuModuleKind::Substrate, "epoch") => RcuModuleKind::Epoch,
            _ => return None,
        };
    }
    Some(kind)
}

fn rcu_module_aliases(
    file: &syn::File,
) -> (
    BTreeMap<(Vec<String>, String), RcuModuleKind>,
    BTreeSet<(Vec<String>, String)>,
) {
    let mut collector = ModuleAliasCollector {
        module_path: Vec::new(),
        bindings: Vec::new(),
    };
    collector.visit_file(file);

    let mut aliases = BTreeMap::new();
    loop {
        let mut changed = false;
        for (module_path, local, target) in &collector.bindings {
            let key = (module_path.clone(), local.clone());
            if aliases.contains_key(&key) {
                continue;
            }
            if let Some(kind) = classify_module_target(target, module_path, &aliases) {
                aliases.insert(key, kind);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut unresolved = BTreeSet::new();
    loop {
        let mut changed = false;
        for (module_path, local, target) in &collector.bindings {
            let key = (module_path.clone(), local.clone());
            if aliases.contains_key(&key) || unresolved.contains(&key) {
                continue;
            }
            if unresolved_module_target(target, module_path, &unresolved) {
                unresolved.insert(key);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    (aliases, unresolved)
}

fn unresolved_module_target(
    target: &[String],
    module_path: &[String],
    unresolved: &BTreeSet<(Vec<String>, String)>,
) -> bool {
    let mut lookup_path = module_path.to_vec();
    let mut start = 0usize;
    let mut relative = false;
    match target.first().map(String::as_str) {
        Some("crate") => {
            lookup_path.clear();
            start = 1;
            relative = true;
        }
        Some("self") => {
            start = 1;
            relative = true;
        }
        Some("super") => {
            relative = true;
            while target.get(start).is_some_and(|segment| segment == "super") {
                if lookup_path.pop().is_none() {
                    start += 1;
                    break;
                }
                start += 1;
            }
        }
        _ => {}
    }

    let Some((first, rest)) = target.get(start..).and_then(|tail| tail.split_first()) else {
        return false;
    };
    if relative && rest.is_empty() {
        return true;
    }
    lookup_scoped_name(unresolved, &lookup_path, first)
        && rest
            .iter()
            .all(|segment| matches!(segment.as_str(), "epoch" | "publication"))
}

struct PublicGlobMacroCollector {
    definitions: BTreeMap<String, TokenStream>,
}

impl<'ast> Visit<'ast> for PublicGlobMacroCollector {
    fn visit_item_macro(&mut self, node: &'ast syn::ItemMacro) {
        if let Some(name) = &node.ident {
            self.definitions
                .insert(name.to_string(), node.mac.tokens.clone());
        }
        visit::visit_item_macro(self, node);
    }
}

fn macro_markers(tokens: TokenStream, markers: &mut (bool, bool, bool)) {
    for token in tokens {
        match token {
            TokenTree::Group(group) => macro_markers(group.stream(), markers),
            TokenTree::Ident(ident) if ident == "pub" => markers.0 = true,
            TokenTree::Ident(ident) if ident == "use" => markers.1 = true,
            TokenTree::Punct(punct) if punct.as_char() == '*' => markers.2 = true,
            TokenTree::Ident(_) | TokenTree::Punct(_) | TokenTree::Literal(_) => {}
        }
    }
}

fn public_glob_macros(file: &syn::File) -> BTreeSet<String> {
    let mut collector = PublicGlobMacroCollector {
        definitions: BTreeMap::new(),
    };
    collector.visit_file(file);

    let mut names = BTreeSet::new();
    for (name, tokens) in &collector.definitions {
        let mut markers = (false, false, false);
        macro_markers(tokens.clone(), &mut markers);
        if markers == (true, true, true) {
            names.insert(name.clone());
        }
    }

    loop {
        let mut changed = false;
        for (name, tokens) in &collector.definitions {
            if !names.contains(name) && macro_calls_any(tokens.clone(), &names) {
                names.insert(name.clone());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    names
}

fn macro_calls_any(tokens: TokenStream, names: &BTreeSet<String>) -> bool {
    let tokens: Vec<_> = tokens.into_iter().collect();
    for (index, token) in tokens.iter().enumerate() {
        match token {
            TokenTree::Group(group) if macro_calls_any(group.stream(), names) => return true,
            TokenTree::Ident(ident)
                if names.contains(&ident.to_string())
                    && tokens.get(index + 1).is_some_and(
                        |next| matches!(next, TokenTree::Punct(punct) if punct.as_char() == '!'),
                    ) =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

struct PrivateTraitCollector {
    module_path: Vec<String>,
    names: BTreeSet<(Vec<String>, String)>,
}

impl<'ast> Visit<'ast> for PrivateTraitCollector {
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if node.content.is_some() {
            self.module_path.push(node.ident.to_string());
            visit::visit_item_mod(self, node);
            self.module_path.pop();
        } else {
            visit::visit_item_mod(self, node);
        }
    }

    fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
        if !visible(&node.vis) {
            self.names
                .insert((self.module_path.clone(), node.ident.to_string()));
        }
        visit::visit_item_trait(self, node);
    }
}

fn private_trait_names(file: &syn::File) -> BTreeSet<(Vec<String>, String)> {
    let mut collector = PrivateTraitCollector {
        module_path: Vec::new(),
        names: BTreeSet::new(),
    };
    collector.visit_file(file);
    collector.names
}

struct PublicationLeakVisitor<'aliases> {
    aliases: &'aliases BTreeMap<String, &'static str>,
    leaks: Vec<(Span, &'static str)>,
}

impl<'ast> Visit<'ast> for PublicationLeakVisitor<'_> {
    fn visit_type_path(&mut self, node: &'ast syn::TypePath) {
        for segment in &node.path.segments {
            if let Some(term) = publication_term(&segment.ident, self.aliases) {
                self.leaks.push((segment.ident.span(), term));
            }
        }
        visit::visit_type_path(self, node);
    }
}

fn publication_leaks(
    aliases: &BTreeMap<String, &'static str>,
    visit_node: impl FnOnce(&mut PublicationLeakVisitor<'_>),
) -> Vec<(Span, &'static str)> {
    let mut visitor = PublicationLeakVisitor {
        aliases,
        leaks: Vec::new(),
    };
    visit_node(&mut visitor);
    visitor.leaks
}

fn code_before_comment(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.is_empty()
        || trimmed.starts_with("//")
        || trimmed.starts_with("///")
        || trimmed.starts_with("//!")
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
    {
        return None;
    }

    let code = line
        .split_once("//")
        .map_or(line, |(before, _)| before)
        .trim();
    (!code.is_empty()).then_some(code)
}

fn contains_ident(code: &str, ident: &str) -> bool {
    let bytes = code.as_bytes();
    let needle = ident.as_bytes();
    if needle.is_empty() || bytes.len() < needle.len() {
        return false;
    }

    for start in 0..=bytes.len() - needle.len() {
        if &bytes[start..start + needle.len()] != needle {
            continue;
        }
        let before = start.checked_sub(1).and_then(|idx| bytes.get(idx).copied());
        let after = bytes.get(start + needle.len()).copied();
        if !is_ident_byte(before) && !is_ident_byte(after) {
            return true;
        }
    }

    false
}

fn is_ident_byte(byte: Option<u8>) -> bool {
    matches!(byte, Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::{RCU_SCAN_ROOTS, enforce_rcu_ratchets, lint_api_language_text};

    #[test]
    fn api_language_flags_adapter_mechanism_terms() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/net/adapter.rs",
            r#"
pub use tx_substrate::zone::{IdentitySlot, ZonePolicy};
pub type ReadyMask = Mask;
"#,
        );

        assert!(findings.iter().any(|finding| {
            finding.bucket == "adapter mechanism language" && finding.term == "IdentitySlot"
        }));
        assert!(findings.iter().any(|finding| {
            finding.bucket == "adapter mechanism language" && finding.term == "Mask"
        }));
    }

    #[test]
    fn api_language_flags_wait_raw_terms_outside_allowed_files() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/net/readiness.rs",
            r#"
fn arm(source_id: WaitSourceId, mask: InterestMask) -> WaitToken {
    source_id.into_raw();
}
"#,
        );

        assert!(findings.iter().any(|finding| {
            finding.bucket == "wait readiness raw language" && finding.term == "WaitSourceId"
        }));
        assert!(findings.iter().any(|finding| {
            finding.bucket == "wait readiness raw language" && finding.term == "source_id"
        }));
    }

    #[test]
    fn api_language_skips_wait_raw_terms_in_allowed_wait_files() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/net/notification.rs",
            "fn notify(token: WaitToken, channel: Channel) { let source_id = 1; }",
        );

        assert!(findings.is_empty());
    }

    #[test]
    fn rcu_ratchet_rejects_any_raw_retire_site() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/vm/structure/recipe.rs",
            "fn retire_one() { let result = unsafe { epoch::retire_raw(ptr, reclaim) }; }",
        );

        assert_eq!(
            findings
                .iter()
                .filter(|finding| finding.bucket == "raw rcu backend language")
                .count(),
            1
        );
        let error = enforce_rcu_ratchets(&findings).expect_err("raw retire must have no exception");
        assert!(error.contains("1 > ceiling 0"));
    }

    #[test]
    fn rcu_ratchet_rejects_manual_atomic_publication_roots() {
        for source in [
            "struct RecipeIndex { root: AtomicPtr<RecipeTree> }",
            "static CURRENT_SNAPSHOT: AtomicPtr<RecipeTree> = AtomicPtr::new(core::ptr::null_mut());",
        ] {
            let findings =
                lint_api_language_text("crates/tx-subsystems/src/vm/structure/recipe.rs", source);
            assert!(findings.iter().any(|finding| {
                finding.bucket == "raw rcu backend language"
                    && finding.term == "AtomicPtr-publication-root"
            }));
            assert!(enforce_rcu_ratchets(&findings).is_err());
        }
    }

    #[test]
    fn rcu_ratchet_allows_nonpublication_atomic_pointer_tables() {
        let findings = lint_api_language_text(
            "crates/tx-reactor/src/runtime.rs",
            "struct ReactorLocals { harts: [AtomicPtr<HartLocal>; 8] }",
        );

        assert!(
            findings
                .iter()
                .all(|finding| finding.term != "AtomicPtr-publication-root")
        );
        assert!(enforce_rcu_ratchets(&findings).is_ok());
    }

    #[test]
    fn rcu_ratchet_rejects_upper_rcu_heads_and_raw_reclaim_callbacks() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/vm/structure/recipe.rs",
            r#"
struct RecipeNode { head: RcuHead }
unsafe fn reclaim_recipe(ptr: *mut RecipeNode) {}
"#,
        );

        assert!(findings.iter().any(|finding| {
            finding.bucket == "raw rcu backend language" && finding.term == "RcuHead"
        }));
        assert!(findings.iter().any(|finding| {
            finding.bucket == "raw rcu backend language" && finding.term == "raw-reclaim-callback"
        }));
        assert!(enforce_rcu_ratchets(&findings).is_err());
    }

    #[test]
    fn rcu_ratchet_rejects_a_second_raw_retire_site() {
        let mut findings = lint_api_language_text(
            "crates/tx-subsystems/src/vm/structure/recipe.rs",
            "fn retire_one() { let result = unsafe { epoch::retire_raw(ptr, reclaim) }; }",
        );
        findings.extend(lint_api_language_text(
            "crates/tx-subsystems/src/page_backed/mod.rs",
            "fn retire_two() { let result = unsafe { epoch::retire_raw(ptr, reclaim) }; }",
        ));

        let error = enforce_rcu_ratchets(&findings).expect_err("second site must exceed ceiling");
        assert!(error.contains("raw retire"));
        assert!(error.contains("2 > ceiling 0"));
    }

    #[test]
    fn rcu_ratchet_counts_two_direct_raw_calls_on_one_line() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/vm/structure/recipe.rs",
            "fn retire_both() { epoch::retire_raw(first, reclaim); epoch::retire_raw(second, reclaim); }",
        );

        let error = enforce_rcu_ratchets(&findings).expect_err("two calls must exceed ceiling");
        assert!(error.contains("2 > ceiling 0"));
    }

    #[test]
    fn rcu_ratchet_rejects_raw_retire_import_alias() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/page_backed/mod.rs",
            r#"
use tx_substrate::epoch::retire_raw as retire;
fn retire_two() {
    retire(first, reclaim);
    retire(second, reclaim);
}
"#,
        );

        let error = enforce_rcu_ratchets(&findings).expect_err("raw import must be forbidden");
        assert!(error.contains("raw retire import or bare call"));
    }

    #[test]
    fn rcu_ratchet_rejects_bare_raw_retire_call() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/page_backed/mod.rs",
            "fn retire_one() { retire_raw(ptr, reclaim); }",
        );

        let error = enforce_rcu_ratchets(&findings).expect_err("bare raw call must be forbidden");
        assert!(error.contains("raw retire import or bare call"));
    }

    #[test]
    fn rcu_ratchet_rejects_publication_backend_in_owner_signature() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/vm/mod.rs",
            "pub fn recipes(owner: &Owner) -> &Published<RecipeTree> { &owner.recipes }",
        );

        assert!(findings.iter().any(|finding| {
            finding.bucket == "publication backend leakage" && finding.term == "Published"
        }));
        let error =
            enforce_rcu_ratchets(&findings).expect_err("owner signature leak must be rejected");
        assert!(error.contains("publication backend"));
        assert!(error.contains("1 > ceiling 0"));
    }

    #[test]
    fn rcu_ratchet_rejects_private_alias_used_in_public_signature() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/vm/mod.rs",
            r#"
use tx_substrate::publication::Published as Snapshot;
pub fn recipes(owner: &Owner) -> &Snapshot<RecipeTree> { &owner.recipes }
"#,
        );

        assert!(findings.iter().any(|finding| {
            finding.bucket == "publication backend leakage" && finding.term == "Published"
        }));
        assert!(enforce_rcu_ratchets(&findings).is_err());
    }

    #[test]
    fn rcu_ratchet_allows_publication_backend_in_owner_private_implementation() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/vm/structure/recipe.rs",
            r#"
use tx_substrate::publication::Published as Snapshot;

struct RecipeIndex {
    root: Snapshot<RecipeTree>,
}

fn recipes(owner: &RecipeIndex) -> &Snapshot<RecipeTree> {
    &owner.root
}
"#,
        );

        assert!(
            findings
                .iter()
                .all(|finding| finding.bucket != "publication backend leakage")
        );
        assert!(enforce_rcu_ratchets(&findings).is_ok());
    }

    #[test]
    fn rcu_ratchet_rejects_raw_retire_method_call() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/page_backed/mod.rs",
            "fn retire_one(owner: &Owner) { owner.retire_raw(ptr, reclaim); }",
        );

        assert!(enforce_rcu_ratchets(&findings).is_err());
    }

    #[test]
    fn rcu_ratchet_rejects_raw_retire_method_definitions() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/page_backed/mod.rs",
            r#"
trait RetireBackend { fn retire_raw(&self); }
impl Owner { fn retire_raw(&self) {} }
extern "C" { fn retire_raw(ptr: *mut u8); }
"#,
        );

        assert!(enforce_rcu_ratchets(&findings).is_err());
        assert_eq!(
            findings
                .iter()
                .filter(|finding| finding.term == "retire_raw-bare")
                .count(),
            3
        );
    }

    #[test]
    fn rcu_ratchet_rejects_raw_retire_function_values_and_parenthesized_calls() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/page_backed/mod.rs",
            r#"
fn retire_both() {
    let retire = epoch::retire_raw;
    retire(first, reclaim);
    retire(second, reclaim);
}
"#,
        );

        let error = enforce_rcu_ratchets(&findings).expect_err("raw alias must be forbidden");
        assert!(error.contains("raw retire import or bare call"));
    }

    #[test]
    fn rcu_ratchet_counts_parenthesized_raw_retire_call() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/page_backed/mod.rs",
            "fn retire_one() { (epoch::retire_raw)(ptr, reclaim); }",
        );

        let error = enforce_rcu_ratchets(&findings)
            .expect_err("parenthesized raw retire must have no exception");
        assert!(error.contains("1 > ceiling 0"));
        assert_eq!(
            findings
                .iter()
                .filter(|finding| finding.term == "retire_raw")
                .count(),
            1
        );
    }

    #[test]
    fn rcu_ratchet_rejects_backend_vocabulary_in_macro_tokens() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/page_backed/mod.rs",
            r#"
macro_rules! expose_backend {
    () => {
        pub fn recipes() -> Published<RecipeTree>;
        epoch::retire_raw(ptr, reclaim);
    };
}
"#,
        );

        let error = enforce_rcu_ratchets(&findings).expect_err("macro bypass must be forbidden");
        assert!(error.contains("publication backend"));
        assert!(error.contains("raw retire import or bare call"));
    }

    #[test]
    fn rcu_ratchet_rejects_module_reexports_generated_by_macros() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/page_backed/mod.rs",
            r#"
macro_rules! expose {
    () => {
        pub use tx_substrate::*;
        pub use tx_substrate::epoch;
    };
}
"#,
        );

        assert!(
            findings
                .iter()
                .any(|finding| finding.bucket == "publication backend leakage")
        );
        assert!(findings.iter().any(|finding| {
            finding.bucket == "raw rcu backend language" && finding.term == "retire_raw-import"
        }));
        assert!(enforce_rcu_ratchets(&findings).is_err());
    }

    #[test]
    fn rcu_ratchet_rejects_bare_substrate_modules_in_macro_arguments() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/page_backed/mod.rs",
            "expose!(tx_substrate);\nexpose!(tx_substrate::epoch);",
        );

        assert!(
            findings
                .iter()
                .any(|finding| finding.bucket == "publication backend leakage")
        );
        assert!(findings.iter().any(|finding| {
            finding.bucket == "raw rcu backend language" && finding.term == "retire_raw-import"
        }));
    }

    #[test]
    fn rcu_ratchet_allows_precise_substrate_items_in_macro_arguments() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/page_backed/mod.rs",
            "expose!(tx_substrate::zone::Cap);\nexpose!(tx_substrate::epoch::guard);",
        );

        assert!(findings.iter().all(|finding| !matches!(
            finding.bucket,
            "raw rcu backend language" | "publication backend leakage"
        )));
    }

    #[test]
    fn rcu_ratchet_rejects_generic_backend_type_in_macro_arguments() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/page_backed/mod.rs",
            "public_wrapper!(Snapshot<RecipeTree>);\nuse tx_substrate::publication::Published as Snapshot;",
        );

        assert!(findings.iter().any(|finding| {
            finding.bucket == "publication backend leakage" && finding.term == "Published"
        }));
        assert!(enforce_rcu_ratchets(&findings).is_err());
    }

    #[test]
    fn rcu_ratchet_rejects_backend_type_alias_in_macro_arguments() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/page_backed/mod.rs",
            "type Root = Published<RecipeTree>;\npublic_wrapper!(Root);",
        );

        assert!(findings.iter().any(|finding| {
            finding.bucket == "publication backend leakage" && finding.term == "Published"
        }));
    }

    #[test]
    fn rcu_ratchet_rejects_bare_published_in_macro_arguments() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/page_backed/mod.rs",
            "public_wrapper!(Published);",
        );

        assert!(findings.iter().any(|finding| {
            finding.bucket == "publication backend leakage" && finding.term == "Published"
        }));
    }

    #[test]
    fn rcu_ratchet_rejects_qualified_backend_path_in_macro_arguments() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/page_backed/mod.rs",
            "public_wrapper!(tx_substrate::publication::Published);",
        );

        assert!(findings.iter().any(|finding| {
            finding.bucket == "publication backend leakage" && finding.term == "Published"
        }));
    }

    #[test]
    fn rcu_ratchet_rejects_public_trait_impl_associated_backend_type() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/page_backed/mod.rs",
            r#"
pub trait SnapshotOwner { type Snapshot; }
impl SnapshotOwner for Owner { type Snapshot = Published<RecipeTree>; }
"#,
        );

        assert!(findings.iter().any(|finding| {
            finding.bucket == "publication backend leakage" && finding.term == "Published"
        }));
        assert!(enforce_rcu_ratchets(&findings).is_err());
    }

    #[test]
    fn rcu_ratchet_allows_private_trait_impl_associated_backend_type() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/page_backed/mod.rs",
            r#"
trait SnapshotOwner { type Snapshot; }
impl SnapshotOwner for Owner { type Snapshot = Published<RecipeTree>; }
"#,
        );

        assert!(
            findings
                .iter()
                .all(|finding| finding.bucket != "publication backend leakage")
        );
    }

    #[test]
    fn rcu_ratchet_does_not_confuse_qualified_trait_with_private_same_name() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/page_backed/mod.rs",
            r#"
trait SnapshotOwner { type Snapshot; }
impl external::SnapshotOwner for Owner { type Snapshot = Published<RecipeTree>; }
"#,
        );

        assert!(findings.iter().any(|finding| {
            finding.bucket == "publication backend leakage" && finding.term == "Published"
        }));
    }

    #[test]
    fn rcu_ratchet_does_not_confuse_nested_private_trait_with_public_same_name() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/page_backed/mod.rs",
            r#"
mod hidden { trait SnapshotOwner { type Snapshot; } }
pub trait SnapshotOwner { type Snapshot; }
impl SnapshotOwner for Owner { type Snapshot = Published<RecipeTree>; }
"#,
        );

        assert!(findings.iter().any(|finding| {
            finding.bucket == "publication backend leakage" && finding.term == "Published"
        }));
    }

    #[test]
    fn rcu_ratchet_rejects_public_inherent_associated_backend_const() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/page_backed/mod.rs",
            "impl Owner { pub const ROOT: Option<Published<RecipeTree>> = None; }",
        );

        assert!(findings.iter().any(|finding| {
            finding.bucket == "publication backend leakage" && finding.term == "Published"
        }));
    }

    #[test]
    fn rcu_ratchet_allows_domain_published_variant_in_macro_arguments() {
        let findings = lint_api_language_text(
            "crates/tx-kernel/src/thread_future/tests.rs",
            "assert_eq!(status, SubmitChildThreadStatus::Published);",
        );

        assert!(
            findings
                .iter()
                .all(|finding| finding.bucket != "publication backend leakage")
        );
    }

    #[test]
    fn rcu_ratchet_ignores_backend_names_in_literals_and_comments() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/vm/mod.rs",
            r##"
pub fn diagnostic() -> &'static str { "Published<RecipeTree> { // not code" }
/*
epoch::retire_raw(ptr, reclaim);
pub fn recipes() -> Published<RecipeTree>;
*/
pub const RAW: &str = r#"PublishReservation<'a, T> }"#;
"##,
        );

        assert!(findings.iter().all(|finding| !matches!(
            finding.bucket,
            "raw rcu backend language" | "publication backend leakage"
        )));
    }

    #[test]
    fn rcu_ratchet_rejects_multiline_publication_backend_signature() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/vm/mod.rs",
            r#"
pub fn recipes(
    owner: &Owner,
) -> &Published<RecipeTree> {
    &owner.recipes
}
"#,
        );

        assert!(findings.iter().any(|finding| {
            finding.bucket == "publication backend leakage" && finding.term == "Published"
        }));
        assert!(enforce_rcu_ratchets(&findings).is_err());
    }

    #[test]
    fn rcu_ratchet_rejects_multiline_publication_backend_reexport() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/vm/mod.rs",
            r#"
pub use tx_substrate::publication::{
    Published,
    PublishError,
};
"#,
        );

        assert_eq!(
            findings
                .iter()
                .filter(|finding| finding.bucket == "publication backend leakage")
                .count(),
            2
        );
        assert!(enforce_rcu_ratchets(&findings).is_err());
    }

    #[test]
    fn rcu_ratchet_rejects_publication_backend_glob_reexport() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/vm/mod.rs",
            "pub use tx_substrate::publication::*;",
        );

        assert_eq!(
            findings
                .iter()
                .filter(|finding| finding.bucket == "publication backend leakage")
                .count(),
            3
        );
        assert!(enforce_rcu_ratchets(&findings).is_err());
    }

    #[test]
    fn rcu_ratchet_rejects_publication_module_reexports() {
        for source in [
            "pub use tx_substrate::publication as backend;",
            "pub use tx_substrate::publication::{self as backend};",
        ] {
            let findings = lint_api_language_text("crates/tx-subsystems/src/vm/mod.rs", source);
            assert!(
                findings
                    .iter()
                    .any(|finding| finding.bucket == "publication backend leakage")
            );
            assert!(enforce_rcu_ratchets(&findings).is_err());
        }
    }

    #[test]
    fn rcu_ratchet_rejects_substrate_root_module_reexports() {
        for source in [
            "pub use tx_substrate as backend;",
            "pub use tx_substrate::{self as backend};",
            "pub use tx_substrate::*;",
        ] {
            let findings = lint_api_language_text("crates/tx-subsystems/src/vm/mod.rs", source);
            assert!(
                findings
                    .iter()
                    .any(|finding| finding.bucket == "publication backend leakage")
            );
            assert!(findings.iter().any(|finding| {
                finding.bucket == "raw rcu backend language" && finding.term == "retire_raw-import"
            }));
            assert!(enforce_rcu_ratchets(&findings).is_err());
        }
    }

    #[test]
    fn rcu_ratchet_rejects_epoch_module_reexports() {
        for source in [
            "pub use tx_substrate::epoch as epoch_api;",
            "pub use tx_substrate::epoch;",
            "pub use tx_substrate::epoch::{self as epoch_api};",
            "pub use tx_substrate::epoch::*;",
        ] {
            let findings = lint_api_language_text("crates/tx-subsystems/src/vm/mod.rs", source);
            assert!(findings.iter().any(|finding| {
                finding.bucket == "raw rcu backend language" && finding.term == "retire_raw-import"
            }));
            assert!(
                findings
                    .iter()
                    .all(|finding| finding.bucket != "publication backend leakage"),
                "epoch re-export misclassified: {findings:#?}"
            );
            assert!(enforce_rcu_ratchets(&findings).is_err());
        }
    }

    #[test]
    fn rcu_ratchet_rejects_private_module_alias_reexports() {
        for source in [
            "use tx_substrate::epoch as e; pub use e::*;",
            "use tx_substrate as ts; pub use ts::*;",
            "use tx_substrate as ts; pub use ts::publication::*;",
        ] {
            let findings = lint_api_language_text("crates/tx-subsystems/src/vm/mod.rs", source);
            assert!(enforce_rcu_ratchets(&findings).is_err(), "{source}");
        }
    }

    #[test]
    fn rcu_ratchet_rejects_extern_crate_alias_reexports() {
        for source in [
            "extern crate tx_substrate as ts; pub use ts::*;",
            "extern crate tx_substrate as ts; pub use ts::publication::*;",
            "pub extern crate tx_substrate as ts;",
        ] {
            let findings = lint_api_language_text("crates/tx-subsystems/src/vm/mod.rs", source);
            assert!(enforce_rcu_ratchets(&findings).is_err(), "{source}");
        }
    }

    #[test]
    fn rcu_ratchet_allows_precise_item_from_private_module_alias() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/vm/mod.rs",
            "use tx_substrate::epoch as e; pub use e::guard;",
        );

        assert!(findings.iter().all(|finding| !matches!(
            finding.bucket,
            "raw rcu backend language" | "publication backend leakage"
        )));
    }

    #[test]
    fn rcu_ratchet_resolves_module_aliases_in_macro_arguments() {
        let rejected = lint_api_language_text(
            "crates/tx-subsystems/src/vm/mod.rs",
            "use tx_substrate::epoch as e; expose!(e);",
        );
        assert!(enforce_rcu_ratchets(&rejected).is_err());

        let allowed = lint_api_language_text(
            "crates/tx-subsystems/src/vm/mod.rs",
            "use tx_substrate::epoch as e; expose!(e::guard);",
        );
        assert!(allowed.iter().all(|finding| !matches!(
            finding.bucket,
            "raw rcu backend language" | "publication backend leakage"
        )));
    }

    #[test]
    fn rcu_ratchet_resolves_relative_module_alias_paths() {
        for source in [
            "use tx_substrate::epoch as e; mod nested { use super::e as local; pub use local::*; }",
            "use tx_substrate::epoch as e; mod nested { use crate::e as local; pub use local::*; }",
            "mod nested { use tx_substrate::epoch as e; use self::e as local; pub use local::*; }",
        ] {
            let findings = lint_api_language_text("crates/tx-subsystems/src/vm/mod.rs", source);
            assert!(enforce_rcu_ratchets(&findings).is_err(), "{source}");
        }
    }

    #[test]
    fn rcu_ratchet_rejects_relative_module_alias_in_macro_argument() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/vm/child.rs",
            "macro_rules! export { ($($p:tt)+) => { pub use $($p)+::*; } } export!(super::e);",
        );

        assert!(enforce_rcu_ratchets(&findings).is_err());
    }

    #[test]
    fn rcu_ratchet_rejects_unresolved_cross_file_relative_glob() {
        for source in [
            "pub use super::e::*;",
            "pub use super::super::e::*;",
            "pub use crate::e::*;",
            "pub use self::e::*;",
        ] {
            let findings = lint_api_language_text("crates/tx-subsystems/src/vm/child.rs", source);
            assert!(enforce_rcu_ratchets(&findings).is_err(), "{source}");
        }
    }

    #[test]
    fn rcu_ratchet_rejects_renamed_cross_file_relative_alias() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/vm/child.rs",
            "use super::e as x; pub use x::*; expose!(x);",
        );

        assert!(enforce_rcu_ratchets(&findings).is_err());
    }

    #[test]
    fn rcu_ratchet_rejects_renamed_cross_file_alias_in_glob_macro() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/vm/child.rs",
            "use super::e as x; macro_rules! export { ($p:path) => { pub use $p::*; } } export!(x);",
        );

        assert!(enforce_rcu_ratchets(&findings).is_err());
    }

    #[test]
    fn rcu_ratchet_propagates_glob_export_through_macro_wrappers() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/vm/child.rs",
            "use super::e as x; macro_rules! do_export { ($p:path) => { pub use $p::*; } } macro_rules! export { ($p:path) => { do_export!($p); } } export!(x);",
        );

        assert!(enforce_rcu_ratchets(&findings).is_err());
    }

    #[test]
    fn rcu_ratchet_allows_explicit_multisegment_facade_glob() {
        let findings = lint_api_language_text(
            "crates/tx-reactor/src/mailbox.rs",
            "pub use crate::adapter::bus_wire::mailbox::*;",
        );

        assert!(findings.iter().all(|finding| !matches!(
            finding.bucket,
            "raw rcu backend language" | "publication backend leakage"
        )));
    }

    #[test]
    fn rcu_ratchet_rejects_publication_backend_in_public_trait_method() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/vm/mod.rs",
            r#"
pub trait RecipeOwner {
    fn recipes(&self) -> &Published<RecipeTree>;
}
"#,
        );

        assert!(findings.iter().any(|finding| {
            finding.bucket == "publication backend leakage" && finding.term == "Published"
        }));
        assert!(enforce_rcu_ratchets(&findings).is_err());
    }

    #[test]
    fn rcu_ratchet_rejects_publication_backend_in_public_enum_variant() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/vm/mod.rs",
            r#"
pub enum RecipeView {
    Current(Published<RecipeTree>),
}
"#,
        );

        assert!(findings.iter().any(|finding| {
            finding.bucket == "publication backend leakage" && finding.term == "Published"
        }));
        assert!(enforce_rcu_ratchets(&findings).is_err());
    }

    #[test]
    fn rcu_ratchet_rejects_single_line_public_enum_backend_variant() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/vm/mod.rs",
            "pub enum RecipeView { Current(Published<RecipeTree>) }",
        );

        assert!(findings.iter().any(|finding| {
            finding.bucket == "publication backend leakage" && finding.term == "Published"
        }));
        assert!(enforce_rcu_ratchets(&findings).is_err());
    }

    #[test]
    fn rcu_ratchet_rejects_single_line_public_trait_backend_method() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/vm/mod.rs",
            "pub trait RecipeOwner { fn recipes(&self) -> &Published<RecipeTree>; }",
        );

        assert!(findings.iter().any(|finding| {
            finding.bucket == "publication backend leakage" && finding.term == "Published"
        }));
        assert!(enforce_rcu_ratchets(&findings).is_err());
    }

    #[test]
    fn rcu_ratchet_allows_unrelated_published_enum_variant_name() {
        let findings = lint_api_language_text(
            "crates/tx-subsystems/src/reactor_submit/mod.rs",
            r#"
pub enum SubmitChildThreadStatus {
    Published,
}
"#,
        );

        assert!(
            findings
                .iter()
                .all(|finding| finding.bucket != "publication backend leakage")
        );
    }

    #[test]
    fn rcu_ratchet_scans_every_declared_production_root() {
        assert_eq!(
            RCU_SCAN_ROOTS,
            [
                "crates/tx-drivers/src",
                "crates/tx-ext4/src",
                "crates/tx-ext4-format/src",
                "crates/tx-fat/src",
                "crates/tx-fat-format/src",
                "crates/tx-fs/src",
                "crates/tx-hal/src",
                "crates/tx-kernel/src",
                "crates/tx-observe/src",
                "crates/tx-observe-types/src",
                "crates/tx-platform-adapter/src",
                "crates/tx-policy/src",
                "crates/tx-reactor/src",
                "crates/tx-scripts/src",
                "crates/tx-services/src",
                "crates/tx-shims/src",
                "crates/tx-subsystems/src",
                "crates/tx-time/src",
                "crates/tx-vdso/src",
                "boards",
            ]
        );
    }
}
