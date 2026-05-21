use std::{
    ops::Range,
    path::{Path, PathBuf},
};

use syn::{spanned::Spanned, File};

use crate::unused::{UnusedDiagnostic, UnusedDiagnosticKind};

const SPACE: u8 = b' ';
const NEWLINE: u8 = b'\n';

pub struct Change {
    file_name: PathBuf,
    original_content: Vec<u8>,
    proposed_content: Vec<u8>,
}

impl Change {
    pub fn file_name(&self) -> &Path {
        &self.file_name
    }

    pub fn original_content(&self) -> &[u8] {
        &self.original_content
    }

    pub fn proposed_content(&self) -> &[u8] {
        &self.proposed_content
    }
}

/// Finds the position of the first whitespace that is considered belonging
/// to the next definition/declaration (this is a kind of "heuristic")
/// Current heuristic:
/// - leave prefixing whitespace as is (if it doesn't contain a newline)
/// - remove the rest of the line (if there is a newline)
fn find_prefix_whitespace(src: &[u8]) -> usize {
    (0..src.len())
        .rev()
        .take_while(|&k| src[k].is_ascii_whitespace())
        .find(|&k| src[k] == NEWLINE)
        .map(|j| j + 1)
        .unwrap_or(src.len())
}

/// Finds the position of the first whitespace that is not considered belonging
/// to the previous definition/declaration (this is kind of "heuristic")
/// Current heuristic:
/// - if there is a newline, eat all space before it, and the newline
/// - if there is no newline, eat all trailing whitespace until the next token
fn find_suffix_whitespace(src: &[u8]) -> usize {
    src.iter()
        .position(|c| *c != SPACE)
        .map(|pos| if src[pos] == NEWLINE { pos + 1 } else { pos })
        .unwrap_or(src.len())
}

/// Turns a list of "locations of identifiers" into a list of "chunks"
fn diagnostics_to_ranges<'a>(
    src: &'a [u8],
    idents: impl IntoIterator<Item = (UnusedDiagnosticKind, String, Option<usize>)> + 'a,
) -> Result<impl Iterator<Item = Range<usize>> + 'a, syn::Error> {
    let s = String::from_utf8_lossy(src);
    let parsed = syn::parse_str::<syn::File>(&s)?;

    let cumulative_lengths = line_offsets(src);

    let ranges = idents
        .into_iter()
        .filter_map(move |(kind, ident, line)| find_item(&parsed.items, &kind, &ident, line))
        .map(move |span| to_range(&cumulative_lengths, span));

    Ok(ranges)
}

/// Walks `items` (the top-level items of a file or the contents of an inline
/// `mod`) for an item matching `kind`, `ident`, and `line`. Descends into
/// `mod`, `extern`, and `impl` blocks. The same walker runs at every depth, so
/// any kind handled at the top level is also handled when nested — the
/// previous split between this and a more limited recursive helper left
/// Const/Enum/Struct/Union/MacroDefinition/ForeignMod/Impl unfindable inside
/// nested modules.
///
/// `line` is the 1-indexed source line rustc pointed at; items on other lines
/// are skipped, disambiguating same-named items in different scopes. `None`
/// disables the line check, which is used by tests with synthetic input.
fn find_item(
    items: &[syn::Item],
    kind: &UnusedDiagnosticKind,
    ident: &str,
    line: Option<usize>,
) -> Option<proc_macro2::Span> {
    use syn::{ForeignItem, ImplItem, Item};
    use UnusedDiagnosticKind::*;

    items.iter().find_map(|item| {
        let item_ident = match item {
            Item::Const(obj) if *kind == Constant => &obj.ident,
            Item::Enum(obj) if *kind == Enum => &obj.ident,
            Item::Fn(obj) if *kind == Function => &obj.sig.ident,
            Item::Macro(syn::ItemMacro {
                ident: Some(name), ..
            }) if *kind == MacroDefinition => name,
            Item::Static(obj) if *kind == Static => &obj.ident,
            Item::Struct(obj) if *kind == Struct => &obj.ident,
            Item::Type(obj) if *kind == TypeAlias => &obj.ident,
            Item::Union(obj) if *kind == Union => &obj.ident,
            Item::Mod(block) => {
                return block
                    .content
                    .as_ref()
                    .and_then(|(_, items)| find_item(items, kind, ident, line));
            }
            Item::ForeignMod(block) => {
                return block.items.iter().find_map(|inner| {
                    let inner_ident = match inner {
                        ForeignItem::Fn(obj) if *kind == Function => &obj.sig.ident,
                        ForeignItem::Static(obj) if *kind == Static => &obj.ident,
                        ForeignItem::Type(obj) if *kind == TypeAlias => &obj.ident,
                        _ => return None,
                    };
                    matches_ident(inner_ident, ident, line).then(|| inner.span())
                });
            }
            Item::Impl(block) => {
                return block.items.iter().find_map(|inner| {
                    let inner_ident = match inner {
                        ImplItem::Const(obj) if *kind == Constant => &obj.ident,
                        ImplItem::Fn(obj) if *kind == AssociatedFunction => &obj.sig.ident,
                        ImplItem::Type(obj) if *kind == TypeAlias => &obj.ident,
                        _ => return None,
                    };
                    matches_ident(inner_ident, ident, line).then(|| inner.span())
                });
            }
            _ => return None,
        };
        matches_ident(item_ident, ident, line).then(|| item.span())
    })
}

fn matches_ident(item_ident: &syn::Ident, ident: &str, line: Option<usize>) -> bool {
    *item_ident == ident && line.is_none_or(|l| item_ident.span().start().line == l)
}

fn expand_ranges_to_include_whitespace<'a>(
    src: &'a [u8],
    iter: impl Iterator<Item = Range<usize>> + 'a,
) -> impl Iterator<Item = Range<usize>> + 'a {
    iter.map(|range| {
        find_prefix_whitespace(&src[..range.start])
            ..find_suffix_whitespace(&src[range.end..]) + range.end
    })
}

/// Deletes a list-of-positions-of-identifiers from a bytearray that is valid
/// rust code BUGS: if the position is in the body of a function, it will try to
/// delete identifiers there ...  probably?
pub fn delete_chunks(src: &[u8], chunks_to_delete: &[Range<usize>]) -> Vec<u8> {
    src.iter()
        .enumerate()
        .filter_map(|(i, &byte)| {
            if chunks_to_delete.iter().any(|range| range.contains(&i)) {
                None
            } else {
                Some(byte)
            }
        })
        .collect()
}

/// Deletes a list-of-positions-of-identifiers from a bytearray that is valid
/// rust code BUGS: if the position is in the body of a function, it will try to
/// delete identifiers there ...  probably?
pub fn rust_delete(
    src: &[u8],
    diagnostics: impl IntoIterator<Item = (UnusedDiagnosticKind, String, Option<usize>)>,
) -> Result<Vec<u8>, syn::Error> {
    let chunks_to_delete =
        expand_ranges_to_include_whitespace(src, diagnostics_to_ranges(src, diagnostics)?);

    Ok(delete_chunks(src, &chunks_to_delete.collect::<Vec<_>>()))
}

/// Processes a list of file+list-of-edits into an iterator of
/// filenames+proposed new contents. Drops files where no actual change was
/// produced — otherwise the diff renderer prints a header and ellipsis with
/// no hunks (the "useless empty diff" output).
fn process_files<Iter: IntoIterator<Item = UnusedDiagnostic>>(
    diagnostics: impl IntoIterator<Item = (PathBuf, Iter)>,
) -> impl Iterator<Item = Change> {
    diagnostics
        .into_iter()
        .filter_map(|(file_name, diagnostic)| {
            let original_content = std::fs::read(&file_name).ok()?;
            let removed_unused = rust_delete(
                &original_content,
                diagnostic
                    .into_iter()
                    .map(|warn| (warn.kind, warn.ident, Some(warn.span.line_start))),
            )
            .expect("syntax error");
            let proposed_content = remove_empty_blocks(&removed_unused).expect("syntax error");

            if original_content == proposed_content {
                return None;
            }

            Some(Change {
                file_name,
                original_content,
                proposed_content,
            })
        })
}

/// Process a list of UnusedDiagnostics into an iterator of filenames+proposed contents
pub fn process_diagnostics(
    diagnostics: impl IntoIterator<Item = UnusedDiagnostic>,
) -> impl Iterator<Item = Change> {
    process_files(
        diagnostics
            .into_iter()
            .map(|diagnostic| {
                let path = PathBuf::from(&diagnostic.span.file_name);
                (path, diagnostic)
            })
            .collect::<multimap::MultiMap<_, _>>(),
    )
}

/// Create a table of byte locations of newline symbols,
/// to translate LineColumn's into exact offsets
fn line_offsets(bytes: &[u8]) -> Vec<usize> {
    let mut offsets: Vec<usize> = bytes
        .iter()
        .enumerate()
        .filter_map(|(pos, b)| match b {
            // TODO: Support \r\n
            b'\n' => Some(pos + 1),
            _ => None,
        })
        .collect();
    // First line has no offset
    offsets.insert(0, 0);

    offsets
}

fn to_range(offsets: &[usize], span: proc_macro2::Span) -> Range<usize> {
    let byte_offset = |pos: proc_macro2::LineColumn| offsets[pos.line - 1] + pos.column;

    byte_offset(span.start())..byte_offset(span.end())
}

fn remove_empty_blocks(bytes: &[u8]) -> Result<Vec<u8>, syn::Error> {
    let s = String::from_utf8_lossy(bytes).to_string();
    let ast: File = syn::parse_str(&s)?;

    let cumulative_lengths = line_offsets(bytes);

    let spans = ast
        .items
        .iter()
        .filter_map(|item| match item {
            syn::Item::ForeignMod(block) => {
                (block.items.is_empty() && block.attrs.is_empty()).then(|| block.span())
            }
            syn::Item::Impl(block) => {
                (block.items.is_empty() && block.attrs.is_empty() && block.trait_.is_none())
                    .then(|| block.span())
            }
            _ => None,
        })
        .map(|span| to_range(&cumulative_lengths, span));

    let expanded_spans: Vec<Range<usize>> =
        expand_ranges_to_include_whitespace(bytes, spans).collect();

    Ok(delete_chunks(bytes, &expanded_spans))
}

/// This actually applies a collection of changes to your filesystem (use with care)
pub fn commit_changes(
    changes: impl IntoIterator<Item = Change>,
) -> Result<(), Vec<std::io::Error>> {
    let errors = changes
        .into_iter()
        .filter_map(|change| std::fs::write(change.file_name, change.proposed_content).err())
        .collect::<Vec<_>>();

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod test {
    use std::sync::atomic::{AtomicU64, Ordering};

    use cargo_metadata::diagnostic::DiagnosticSpan;

    use super::*;

    // Tests pass `None` for the line — the synthetic inputs use unique names,
    // so disambiguation isn't needed and we avoid hard-coding line numbers.
    fn fun(name: &str) -> (UnusedDiagnosticKind, String, Option<usize>) {
        (UnusedDiagnosticKind::Function, name.to_owned(), None)
    }

    fn constant(name: &str) -> (UnusedDiagnosticKind, String, Option<usize>) {
        (UnusedDiagnosticKind::Constant, name.to_owned(), None)
    }

    fn struct_(name: &str) -> (UnusedDiagnosticKind, String, Option<usize>) {
        (UnusedDiagnosticKind::Struct, name.to_owned(), None)
    }

    fn enum_(name: &str) -> (UnusedDiagnosticKind, String, Option<usize>) {
        (UnusedDiagnosticKind::Enum, name.to_owned(), None)
    }

    fn union_(name: &str) -> (UnusedDiagnosticKind, String, Option<usize>) {
        (UnusedDiagnosticKind::Union, name.to_owned(), None)
    }

    fn macro_(name: &str) -> (UnusedDiagnosticKind, String, Option<usize>) {
        (UnusedDiagnosticKind::MacroDefinition, name.to_owned(), None)
    }

    fn assoc_fn(name: &str) -> (UnusedDiagnosticKind, String, Option<usize>) {
        (
            UnusedDiagnosticKind::AssociatedFunction,
            name.to_owned(),
            None,
        )
    }

    // Writes `content` to a uniquely-named file under the OS temp dir and
    // removes it on drop. Used by tests that exercise `process_diagnostics`,
    // which reads the source from disk.
    struct TempRust(PathBuf);

    impl TempRust {
        fn new(content: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "cargo-minify-test-{}-{}.rs",
                std::process::id(),
                n
            ));
            std::fs::write(&path, content).unwrap();
            TempRust(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRust {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn diag(file: &Path, line: usize, kind: UnusedDiagnosticKind, ident: &str) -> UnusedDiagnostic {
        // DiagnosticSpan is #[non_exhaustive] so we can't construct it with a
        // struct literal. Deserialize from JSON instead.
        let span: DiagnosticSpan = serde_json::from_value(serde_json::json!({
            "file_name": file.to_string_lossy(),
            "byte_start": 0,
            "byte_end": 0,
            "line_start": line,
            "line_end": line,
            "column_start": 1,
            "column_end": 1,
            "is_primary": true,
            "text": [],
            "label": null,
            "suggested_replacement": null,
            "suggestion_applicability": null,
            "expansion": null,
        }))
        .unwrap();
        UnusedDiagnostic {
            kind,
            ident: ident.to_owned(),
            span,
        }
    }

    #[test]
    fn identifier_to_span() {
        let src = b"fn foo() {}  fn foa() -> i32 { barf; } const FOO: i32 = 42;";
        //          01234567890123456789012345678901234567890123456789012345678
        //                    1         2         3         4         5
        let pos = diagnostics_to_ranges(src, [fun("foo"), fun("foa"), constant("FOO")])
            .unwrap()
            .collect::<Vec<_>>();
        assert_eq!(pos, vec![0..11, 13..38, 39..59]);
    }

    #[allow(clippy::single_range_in_vec_init)]
    #[test]
    fn chunk_deletion() {
        let src = b"fn foo() {}  fn foa() -> i32 { barf; } const FOO: i32 = 42;";
        //          012345678901234567890123456789012345678901234567890123456
        assert_eq!(
            delete_chunks(src, &[5..8]),
            b"fn fo {}  fn foa() -> i32 { barf; } const FOO: i32 = 42;"
        );
    }

    #[test]
    fn deletion() {
        let src = b"fn foo() { }fn foa() -> i32 { barf; }const FOO: i32 = 42;";
        assert_eq!(
            rust_delete(src, [fun("foo")]).unwrap(),
            b"fn foa() -> i32 { barf; }const FOO: i32 = 42;"
        );
        assert_eq!(
            rust_delete(src, [fun("foa")]).unwrap(),
            b"fn foo() { }const FOO: i32 = 42;"
        );
        assert_eq!(
            rust_delete(src, [constant("FOO")]).unwrap(),
            b"fn foo() { }fn foa() -> i32 { barf; }"
        );
    }

    #[test]
    fn type_check() {
        let src = b"fn foo() { }fn foa() -> i32 { barf; }const FOO: i32 = 42;";
        assert_eq!(
            rust_delete(src, [constant("foo")]).unwrap(),
            b"fn foo() { }fn foa() -> i32 { barf; }const FOO: i32 = 42;"
        );
        assert_eq!(
            rust_delete(src, [constant("foa")]).unwrap(),
            b"fn foo() { }fn foa() -> i32 { barf; }const FOO: i32 = 42;"
        );
        assert_eq!(
            rust_delete(src, [fun("FOO")]).unwrap(),
            b"fn foo() { }fn foa() -> i32 { barf; }const FOO: i32 = 42;"
        );
    }

    #[test]
    fn formatting_preserval() {
        let src = b" fn foo(){}  fn foa()  -> huk {  barf; }   const FOO: i32 = 42;  fn bar(){ } ";
        assert_eq!(
            rust_delete(src, [fun("foo")]).unwrap(),
            b" fn foa()  -> huk {  barf; }   const FOO: i32 = 42;  fn bar(){ } "
        );
        assert_eq!(
            rust_delete(src, [fun("foa")]).unwrap(),
            b" fn foo(){}  const FOO: i32 = 42;  fn bar(){ } "
        );
        assert_eq!(
            rust_delete(src, [constant("FOO")]).unwrap(),
            b" fn foo(){}  fn foa()  -> huk {  barf; }   fn bar(){ } "
        );
        assert_eq!(
            rust_delete(src, [fun("bar")]).unwrap(),
            b" fn foo(){}  fn foa()  -> huk {  barf; }   const FOO: i32 = 42;  "
        );

        assert_eq!(
            rust_delete(src, [fun("foa"), fun("foo")]).unwrap(),
            b" const FOO: i32 = 42;  fn bar(){ } "
        );
        assert_eq!(
            rust_delete(src, [fun("foa"), constant("FOO")]).unwrap(),
            b" fn foo(){}  fn bar(){ } "
        );
    }

    #[test]
    #[rustfmt::skip]
    fn whitespace_semi_preserval() {
        let src = b" fn foo() {} fn fixme() {} fn main() {}";
        assert_eq!(
            rust_delete(src, [fun("fixme")]).unwrap(),
            b" fn foo() {} fn main() {}"
        );
        let src = b" fn foo() {} fn fixme() {}fn main() {}";
        assert_eq!(
            rust_delete(src, [fun("fixme")]).unwrap(),
            b" fn foo() {} fn main() {}"
        );
        let src = b" fn foo() {}fn fixme() {} fn main() {}";
        assert_eq!(
            rust_delete(src, [fun("fixme")]).unwrap(),
            b" fn foo() {}fn main() {}"
        );
        let src = b" fn foo() {}\nfn fixme() {}\nfn main() {}";
        assert_eq!(
            rust_delete(src, [fun("fixme")]).unwrap(),
            b" fn foo() {}\nfn main() {}"
        );
        let src = b" fn foo() {}\n\nfn fixme() {}\nfn main() {}";
        assert_eq!(
            rust_delete(src, [fun("fixme")]).unwrap(),
            b" fn foo() {}\n\nfn main() {}"
        );
        let src = b" fn foo() {}\nfn fixme() {}\n\nfn main() {}";
        assert_eq!(
            rust_delete(src, [fun("fixme")]).unwrap(),
            b" fn foo() {}\n\nfn main() {}"
        );
        let src = b" fn foo() {}\n\nfn fixme() {}\n\nfn main() {}";
        assert_eq!(
            rust_delete(src, [fun("fixme")]).unwrap(),
            b" fn foo() {}\n\n\nfn main() {}"
        );

        let src = b"fn foo() {}\n          fn fixme() {}\n   fn main() {}";
        assert_eq!(
            rust_delete(src, [fun("fixme")]).unwrap(),
            b"fn foo() {}\n   fn main() {}"
        );
    }

    // The walker in handle_mod_diagnostic used to only recognize Fn/Static/Type
    // inside nested modules. Every other kind that the top-level walker handles
    // (Const/Enum/Struct/Union/MacroDefinition, plus AssociatedFunction inside
    // an Impl block) was silently skipped — producing a no-op deletion.
    // cargo-equip bundles are full of nested modules, so this gap meant
    // cargo-minify could not act on warnings rustc emitted for items in them.

    #[test]
    fn nested_const_deletion() {
        let src = b"mod m { const FOO: i32 = 1; }";
        assert_eq!(rust_delete(src, [constant("FOO")]).unwrap(), b"mod m { }");
    }

    #[test]
    fn nested_enum_deletion() {
        let src = b"mod m { enum E {} }";
        assert_eq!(rust_delete(src, [enum_("E")]).unwrap(), b"mod m { }");
    }

    #[test]
    fn nested_struct_deletion() {
        let src = b"mod m { struct S; }";
        assert_eq!(rust_delete(src, [struct_("S")]).unwrap(), b"mod m { }");
    }

    #[test]
    fn nested_union_deletion() {
        let src = b"mod m { union U { a: i32 } }";
        assert_eq!(rust_delete(src, [union_("U")]).unwrap(), b"mod m { }");
    }

    #[test]
    fn nested_macro_definition_deletion() {
        let src = b"mod m { macro_rules! mac { () => {} } }";
        assert_eq!(rust_delete(src, [macro_("mac")]).unwrap(), b"mod m { }");
    }

    // Mirrors the cargo-equip bundle structure: __cargo_equip::crates::<dep>
    // containing the items rustc warns about.
    #[test]
    fn deeply_nested_macro_definition_deletion() {
        let src = b"mod outer { mod inner { macro_rules! mac { () => {} } } }";
        assert_eq!(
            rust_delete(src, [macro_("mac")]).unwrap(),
            b"mod outer { mod inner { } }"
        );
    }

    #[test]
    fn nested_assoc_fn_deletion() {
        let src = b"mod m { struct S; impl S { fn unused() {} } }";
        assert_eq!(
            rust_delete(src, [assoc_fn("unused")]).unwrap(),
            b"mod m { struct S; impl S { } }"
        );
    }

    // process_files used to emit a Change unconditionally, even when
    // rust_delete couldn't locate the item and returned the source bytes
    // unchanged. That produced the "useless empty diff" output where
    // diff_format printed only the header and an ellipsis.
    #[test]
    fn process_diagnostics_drops_no_op_change() {
        let tmp = TempRust::new("fn used() {}\n");
        let d = diag(tmp.path(), 1, UnusedDiagnosticKind::Function, "missing");
        let changes: Vec<_> = process_diagnostics([d]).collect();
        assert!(
            changes.is_empty(),
            "expected zero changes, got {}",
            changes.len()
        );
    }

    // cauterize used to match items by ident alone. When two items share a
    // name in different scopes (common in cargo-equip bundles where each
    // vendored crate re-declares `mod macros` etc.), it picked the first
    // lexical match regardless of which one rustc actually warned about.
    // The diagnostic's line_start should disambiguate.
    #[test]
    fn process_diagnostics_disambiguates_duplicate_names_by_line() {
        let tmp = TempRust::new("mod m { fn foo() {} }\nfn foo() {}\n");

        // Diagnostic at line 2 → should remove the top-level foo only.
        let d = diag(tmp.path(), 2, UnusedDiagnosticKind::Function, "foo");
        let changes: Vec<_> = process_diagnostics([d]).collect();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].proposed_content(), b"mod m { fn foo() {} }\n");
    }

    #[test]
    fn process_diagnostics_disambiguates_duplicate_names_by_line_nested() {
        let tmp = TempRust::new("mod m { fn foo() {} }\nfn foo() {}\n");

        // Diagnostic at line 1 → should remove only the nested foo.
        let d = diag(tmp.path(), 1, UnusedDiagnosticKind::Function, "foo");
        let changes: Vec<_> = process_diagnostics([d]).collect();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].proposed_content(), b"mod m { }\nfn foo() {}\n");
    }
}
