//! Source digests for the extractor fingerprint, computed by build scripts: tycho-ethereum's
//! `build.rs` uses this file for `SERVICES_SOURCE_DIGEST`. It reads only the files it is given, so
//! it runs from the published package too.
//!
//! A digest ignores edits that cannot change the code's behaviour. For each `.rs` file it hashes
//! the token stream that `syn` parses, without items marked `#[cfg(test)]`, functions marked
//! `#[test]` (or `#[tokio::test]` and similar) and doc attributes, so comments, docs, formatting
//! and inline test code do not count; any other token does, a renamed variable included. Other
//! files (Solidity sources, bytecode, JSON) are hashed as they are, with CRLF normalized to LF when
//! they are UTF-8.
//!
//! The token stream is hashed in its string form. An upgrade of `syn`, `quote` or `proc-macro2`
//! can change that form, and with it every digest, once.

use std::{fs, path::Path};

use proc_macro2::{Delimiter, Group, TokenStream, TokenTree};
use quote::ToTokens;
use syn::{
    visit_mut::{self, VisitMut},
    Attribute, Block, File, ImplItem, Item, ItemImpl, ItemMod, ItemTrait, Stmt, TraitItem,
};
use tiny_keccak::{Hasher, Keccak};

/// Hex-encoded Keccak-256 digest of the files at `paths` (files, or directories walked
/// recursively), relative to `root`: every file's relative path and [`normalized`] contents, in
/// sorted path order, each prefixed with its length.
///
/// # Panics
/// If a path does not exist, a file cannot be read, or a `.rs` file does not parse.
pub fn source_digest(root: &Path, paths: &[&str]) -> String {
    let mut files = Vec::new();
    for path in paths {
        collect_files(root, path, &mut files);
    }
    files.sort();

    let mut hasher = Keccak::v256();
    for relative in &files {
        let path = root.join(relative);
        let contents =
            fs::read(&path).unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()));
        update_framed(&mut hasher, relative.as_bytes());
        update_framed(&mut hasher, &normalized(relative, contents));
    }
    let mut digest = [0u8; 32];
    hasher.finalize(&mut digest);
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The bytes hashed for the file at `relative`, given its `contents`:
/// - a `.rs` file: its token stream as a string, without `#[cfg(test)]` items, `#[test]` functions
///   and doc attributes;
/// - another UTF-8 file: its contents with CRLF normalized to LF, so a checkout that converts line
///   endings (Git on Windows) gives the same digest;
/// - anything else, such as compiled bytecode: its contents.
///
/// # Panics
/// If a `.rs` file is not UTF-8 or does not parse: a digest that silently hashed it raw would
/// change with its comments again.
pub fn normalized(relative: &str, contents: Vec<u8>) -> Vec<u8> {
    let is_rust = relative.ends_with(".rs");
    let text = match String::from_utf8(contents) {
        Ok(text) => text.replace("\r\n", "\n"),
        Err(err) if is_rust => panic!("{relative} is not UTF-8: {err}"),
        Err(err) => return err.into_bytes(),
    };
    if !is_rust {
        return text.into_bytes();
    }
    let mut file: File = syn::parse_file(&text)
        .unwrap_or_else(|err| panic!("cannot parse {relative} as Rust: {err}"));
    WithoutTestItems.visit_file_mut(&mut file);
    without_doc_attributes(file.into_token_stream())
        .to_string()
        .into_bytes()
}

/// Appends the files at `relative` (a file, or a directory walked recursively) to `files`, as
/// paths relative to `root` joined with `/` on every platform. Skips hidden files and editor
/// backups (names starting with `.` or ending with `~`), which are not sources.
///
/// # Panics
/// If `relative` does not exist, or holds a name that is not UTF-8.
fn collect_files(root: &Path, relative: &str, files: &mut Vec<String>) {
    let path = root.join(relative);
    let metadata = fs::metadata(&path).unwrap_or_else(|err| {
        panic!("cannot read {}: {err}. Update HASHED_PATHS in build.rs.", path.display())
    });
    if !metadata.is_dir() {
        files.push(relative.to_owned());
        return;
    }
    let entries =
        fs::read_dir(&path).unwrap_or_else(|err| panic!("cannot list {}: {err}", path.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|err| panic!("cannot list {}: {err}", path.display()));
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            panic!("{} holds a file name that is not UTF-8: {name:?}", path.display());
        };
        if name.starts_with('.') || name.ends_with('~') {
            continue;
        }
        collect_files(root, &format!("{relative}/{name}"), files);
    }
}

/// Hashes `bytes` prefixed with their length, so moving bytes from one field to the next changes
/// the digest.
fn update_framed(hasher: &mut Keccak, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

/// Removes the items only test builds compile, wherever items appear: modules, impl and trait
/// blocks, and function bodies.
struct WithoutTestItems;

impl VisitMut for WithoutTestItems {
    fn visit_file_mut(&mut self, file: &mut File) {
        file.items
            .retain(|item| !is_test_only(item_attributes(item)));
        visit_mut::visit_file_mut(self, file);
    }

    fn visit_item_mod_mut(&mut self, module: &mut ItemMod) {
        if let Some((_, items)) = &mut module.content {
            items.retain(|item| !is_test_only(item_attributes(item)));
        }
        visit_mut::visit_item_mod_mut(self, module);
    }

    fn visit_item_impl_mut(&mut self, block: &mut ItemImpl) {
        block
            .items
            .retain(|item| !is_test_only(impl_item_attributes(item)));
        visit_mut::visit_item_impl_mut(self, block);
    }

    fn visit_item_trait_mut(&mut self, block: &mut ItemTrait) {
        block
            .items
            .retain(|item| !is_test_only(trait_item_attributes(item)));
        visit_mut::visit_item_trait_mut(self, block);
    }

    fn visit_block_mut(&mut self, block: &mut Block) {
        block.stmts.retain(|stmt| match stmt {
            Stmt::Item(item) => !is_test_only(item_attributes(item)),
            Stmt::Local(_) | Stmt::Expr(..) | Stmt::Macro(_) => true,
        });
        visit_mut::visit_block_mut(self, block);
    }
}

/// Whether `attributes` make their item exist only in test builds: `#[cfg(test)]`, or a test
/// attribute such as `#[test]`, `#[tokio::test]` or `#[actix_rt::test]`.
fn is_test_only(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        let path = attribute.path();
        let cfg_test = path.is_ident("cfg") &&
            attribute
                .parse_args::<syn::Ident>()
                .is_ok_and(|predicate| predicate == "test");
        let test = path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "test");
        cfg_test || test
    })
}

fn item_attributes(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(item) => &item.attrs,
        Item::Enum(item) => &item.attrs,
        Item::ExternCrate(item) => &item.attrs,
        Item::Fn(item) => &item.attrs,
        Item::ForeignMod(item) => &item.attrs,
        Item::Impl(item) => &item.attrs,
        Item::Macro(item) => &item.attrs,
        Item::Mod(item) => &item.attrs,
        Item::Static(item) => &item.attrs,
        Item::Struct(item) => &item.attrs,
        Item::Trait(item) => &item.attrs,
        Item::TraitAlias(item) => &item.attrs,
        Item::Type(item) => &item.attrs,
        Item::Union(item) => &item.attrs,
        Item::Use(item) => &item.attrs,
        // Tokens syn does not parse into an item, and variants added after syn 2.0.117 (`Item`
        // is non-exhaustive): kept, so they are hashed.
        _ => &[],
    }
}

fn impl_item_attributes(item: &ImplItem) -> &[Attribute] {
    match item {
        ImplItem::Const(item) => &item.attrs,
        ImplItem::Fn(item) => &item.attrs,
        ImplItem::Type(item) => &item.attrs,
        ImplItem::Macro(item) => &item.attrs,
        // As in `item_attributes`.
        _ => &[],
    }
}

fn trait_item_attributes(item: &TraitItem) -> &[Attribute] {
    match item {
        TraitItem::Const(item) => &item.attrs,
        TraitItem::Fn(item) => &item.attrs,
        TraitItem::Type(item) => &item.attrs,
        TraitItem::Macro(item) => &item.attrs,
        // As in `item_attributes`.
        _ => &[],
    }
}

/// `tokens` without doc attributes, outer (`#[doc = ...]`, which `///` comments become) and inner
/// (`#![doc = ...]`, from `//!`), at any depth, macro invocations included.
fn without_doc_attributes(tokens: TokenStream) -> TokenStream {
    let trees: Vec<TokenTree> = tokens.into_iter().collect();
    let mut kept = Vec::with_capacity(trees.len());
    let mut index = 0;
    while index < trees.len() {
        if let Some(end) = doc_attribute_end(&trees, index) {
            index = end;
            continue;
        }
        kept.push(match &trees[index] {
            TokenTree::Group(group) => {
                let mut stripped =
                    Group::new(group.delimiter(), without_doc_attributes(group.stream()));
                stripped.set_span(group.span());
                TokenTree::Group(stripped)
            }
            tree @ (TokenTree::Ident(_) | TokenTree::Punct(_) | TokenTree::Literal(_)) => {
                tree.clone()
            }
        });
        index += 1;
    }
    kept.into_iter().collect()
}

/// When `trees[start..]` begins with a doc attribute (`#`, an optional `!`, then a bracketed group
/// starting with `doc`), the index right after it.
fn doc_attribute_end(trees: &[TokenTree], start: usize) -> Option<usize> {
    let TokenTree::Punct(hash) = trees.get(start)? else {
        return None;
    };
    if hash.as_char() != '#' {
        return None;
    }
    let mut group_index = start + 1;
    if let Some(TokenTree::Punct(bang)) = trees.get(group_index) {
        if bang.as_char() == '!' {
            group_index += 1;
        }
    }
    let TokenTree::Group(group) = trees.get(group_index)? else {
        return None;
    };
    if group.delimiter() != Delimiter::Bracket {
        return None;
    }
    match group.stream().into_iter().next() {
        Some(TokenTree::Ident(name)) if name == "doc" => Some(group_index + 1),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"//! Module docs.

/// The limit.
pub const LIMIT: u32 = 5;

/// Doubles `value`.
pub fn double(value: u32) -> u32 {
    // Why it doubles.
    value * 2
}

alloy::sol! {
    /// The token interface.
    interface IToken {
        function balanceOf(address owner) external view returns (uint256);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn doubles() {
        assert_eq!(super::double(2), 4);
    }
}
"#;

    fn normalized_source(source: &str) -> Vec<u8> {
        normalized("src/services/example.rs", source.as_bytes().to_vec())
    }

    #[test]
    fn edits_that_cannot_change_behaviour_keep_the_normalized_source() {
        let edits = [
            ("a comment", SOURCE.replace("// Why it doubles.", "// Twice the value.")),
            ("a doc comment", SOURCE.replace("/// Doubles `value`.", "/// Twice `value`.")),
            ("a doc attribute", SOURCE.replace("/// The limit.", "#[doc = \"Limit.\"]")),
            ("an inner doc comment", SOURCE.replace("//! Module docs.", "//! Other docs.")),
            (
                "a doc comment in a macro invocation",
                SOURCE.replace("/// The token interface.", "/// An ERC-20 interface."),
            ),
            ("the formatting", SOURCE.replace("value * 2", "value\n        *\n        2")),
            ("CRLF line endings", SOURCE.replace('\n', "\r\n")),
            ("a test module", SOURCE.replace("4);", "4, \"doubled\");")),
            (
                "a test function outside a test module",
                format!("{SOURCE}\n#[tokio::test]\nasync fn works() {{ assert!(true); }}\n"),
            ),
        ];
        let base = normalized_source(SOURCE);
        for (edit, source) in edits {
            assert_eq!(normalized_source(&source), base, "changing {edit} changed the source");
        }

        // Test-only items go wherever they are, not only at the top level.
        let with_test_items = format!(
            "{SOURCE}\nimpl Limit {{ #[cfg(test)] fn new() {{}} }}\nfn run() {{ #[cfg(test)] fn \
             helper() {{}} }}\n"
        );
        let without_test_items = format!("{SOURCE}\nimpl Limit {{}}\nfn run() {{}}\n");
        assert_eq!(normalized_source(&with_test_items), normalized_source(&without_test_items));
    }

    #[test]
    fn edits_to_code_change_the_normalized_source() {
        let edits = [
            ("a constant", SOURCE.replace("LIMIT: u32 = 5", "LIMIT: u32 = 6")),
            ("an expression", SOURCE.replace("value * 2", "value * 3")),
            ("a macro invocation", SOURCE.replace("address owner", "address account")),
            ("a cfg other than test", SOURCE.replace("#[cfg(test)]", "#[cfg(not(test))]")),
            ("a string literal", format!("{SOURCE}\nconst NAME: &str = \"a\";\n")),
        ];
        let base = normalized_source(SOURCE);
        for (edit, source) in edits {
            assert_ne!(normalized_source(&source), base, "changing {edit} kept the source");
        }
    }

    #[test]
    fn files_that_are_not_rust_keep_their_bytes_except_line_endings() {
        assert_eq!(normalized("contracts/A.sol", b"a\r\nb\n".to_vec()), b"a\nb\n");
        let bytecode = vec![0x60, 0x80, b'\r', b'\n', 0xff];
        assert_eq!(normalized("assets/router.evm.runtime", bytecode.clone()), bytecode);
    }

    #[test]
    #[should_panic(expected = "cannot parse src/broken.rs as Rust")]
    fn a_rust_file_that_does_not_parse_fails() {
        normalized("src/broken.rs", b"fn broken( {".to_vec());
    }

    /// The digest of the fixture `the_digest_of_a_fixed_fixture_is_pinned` writes.
    const GOLDEN_DIGEST: &str = "0170021eb5696e346b479ee98544116c1e2491041f434f5844d90eeae47fc0b5";

    /// Pins the digest of a fixed fixture: a Rust file with comments, docs and a test module, a
    /// UTF-8 file with CRLF line endings in a subdirectory, a binary file and a hidden file. Any
    /// change to how the digest is computed, an upgrade of `syn`, `quote` or `proc-macro2`
    /// included, fails this test until someone accepts it.
    #[test]
    fn the_digest_of_a_fixed_fixture_is_pinned() {
        let root =
            std::env::temp_dir().join(format!("source-digest-fixture-{}", std::process::id()));
        // A directory left by an earlier run that panicked would add files to the fixture.
        if root.exists() {
            fs::remove_dir_all(&root).expect("a stale fixture directory can be removed");
        }
        let contracts = root.join("src/contracts");
        fs::create_dir_all(&contracts).expect("the fixture directory can be created");
        fs::write(root.join("src/lib.rs"), SOURCE).expect("the fixture is writable");
        fs::write(contracts.join("Token.sol"), "contract Token {}\r\n").expect("writable");
        fs::write(contracts.join("router.evm.runtime"), [0x60, 0x80, b'\r', b'\n', 0xff])
            .expect("writable");
        fs::write(root.join("src/.DS_Store"), "skipped").expect("writable");

        let digest = source_digest(&root, &["src"]);
        fs::remove_dir_all(&root).expect("the fixture directory can be removed");

        assert_eq!(
            digest, GOLDEN_DIGEST,
            "The digest of the fixed fixture changed. An upgrade of syn, quote or proc-macro2, or a \
             change to how build/source_digest.rs normalizes files, changes every source digest, \
             and with it every extractor fingerprint, once: every client resyncs from a snapshot \
             after the next deploy. If that is intended, accept it by setting GOLDEN_DIGEST to \
             {digest}. Otherwise the change is a bug."
        );
    }
}
