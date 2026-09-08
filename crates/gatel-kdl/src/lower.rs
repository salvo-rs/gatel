//! Lowering KDL syntax into the format-neutral node tree.
//!
//! This frontend is the only layer in Gatel that knows KDL exists. Everything after it
//! sees [`gatel_model::Node`], which is why the Admin API and a future
//! Kubernetes frontend reuse every decoder unchanged.

use gatel_model::{Code, Diagnostic, Diagnostics, Node, Origin, Property, Scalar, Span, Value};
use kdl::{KdlDocument, KdlNode, KdlValue};

/// KDL v2's eight newline sequences. CRLF is a single line ending.
fn newline_len(bytes: &[u8]) -> usize {
    match bytes {
        [b'\r', b'\n', ..] => 2,
        [b'\r' | b'\n' | 0x0b | 0x0c, ..] => 1,
        [0xc2, 0x85, ..] => 2,
        [0xe2, 0x80, 0xa8 | 0xa9, ..] => 3,
        _ => 0,
    }
}

/// Maps byte offsets to line and column, computed once per file.
pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut starts = vec![0];
        let mut at = 0;
        while at < text.len() {
            let length = newline_len(&text.as_bytes()[at..]);
            if length == 0 {
                at += 1;
            } else {
                at += length;
                starts.push(at);
            }
        }
        Self { starts }
    }

    pub fn span(&self, offset: usize, length: usize) -> Span {
        let line = self.starts.partition_point(|start| *start <= offset).max(1);
        let column = offset - self.starts[line - 1] + 1;
        Span::new(offset, length, line as u32, column as u32)
    }
}

/// Parse KDL text into top-level nodes.
///
/// Every returned node carries an [`Origin`] pointing back into `file`, so a
/// diagnostic raised twelve layers later still knows the line and column.
pub fn lower(text: &str, file: &str) -> Result<Vec<Node>, Diagnostics> {
    let index = LineIndex::new(text);
    let document = parse(text, file)?;
    Ok(document
        .nodes()
        .iter()
        .map(|node| lower_node(node, file, &index))
        .collect())
}

/// Shared guarded syntax entry point for lowering and formatting.
pub(crate) fn parse(text: &str, file: &str) -> Result<KdlDocument, Diagnostics> {
    let index = LineIndex::new(text);

    // Checked before parsing, not after. The KDL parser is recursive, and a
    // file nested deep enough exhausts the stack — which aborts the process
    // rather than raising anything, so nothing downstream would ever get the
    // chance to reject it. A gateway that dies because of an edit is exactly
    // what the reload transaction exists to prevent, and "the configuration is
    // trusted" is not an answer when the failure is a typo repeated.
    if let Some(offset) = too_deep(text) {
        let mut diagnostics = Diagnostics::new();
        diagnostics.push(
            Diagnostic::error(
                Code::SyntaxError,
                Origin::file(file.to_string(), index.span(offset, 1)),
                format!("nesting is deeper than {MAX_NESTING} levels (nodes or disable markers) or a block comment exceeds {MAX_COMMENT_STEPS} parser steps"),
            )
            .with_help("reduce nesting or split complex block comments into line comments"),
        );
        return Err(diagnostics);
    }

    let document = match KdlDocument::parse(text) {
        Ok(document) => document,
        Err(error) => {
            let mut diagnostics = Diagnostics::new();
            for diagnostic in &error.diagnostics {
                let offset = diagnostic.span.offset();
                let origin = Origin::file(file.to_string(), index.span(offset, diagnostic.span.len()));
                let message = diagnostic
                    .message
                    .clone()
                    .unwrap_or_else(|| "could not parse KDL here".to_string());
                let mut item = Diagnostic::error(Code::SyntaxError, origin, message);
                if let Some(help) = &diagnostic.help {
                    item = item.with_help(help.clone());
                }
                diagnostics.push(item);
            }
            if diagnostics.is_empty() {
                diagnostics.error(
                    Code::SyntaxError,
                    Origin::file(file.to_string(), Span::default()),
                    "could not parse KDL",
                );
            }
            return Err(diagnostics);
        }
    };
    Ok(document)
}

fn lower_node(node: &KdlNode, file: &str, index: &LineIndex) -> Node {
    let span = node.span();
    let origin = Origin::file(file.to_string(), index.span(span.offset(), span.len()));
    let mut lowered = Node::new(node.name().value(), origin);

    for entry in node.entries() {
        let entry_span = entry.span();
        let entry_origin = Origin::file(
            file.to_string(),
            index.span(entry_span.offset(), entry_span.len()),
        );
        let value = Value::new(lower_value(entry.value()), entry_origin.clone());
        match entry.name() {
            Some(name) => lowered.props.push(Property {
                name: name.value().to_string(),
                value,
                origin: entry_origin,
            }),
            None => lowered.args.push(value),
        }
    }

    if let Some(children) = node.children() {
        lowered.children = children
            .nodes()
            .iter()
            .map(|child| lower_node(child, file, index))
            .collect();
    }
    lowered
}

fn lower_value(value: &KdlValue) -> Scalar {
    match value {
        KdlValue::String(text) => Scalar::Str(text.clone()),
        // KDL integers are i128; anything outside i64 is out of range for every
        // field Gatel has, and is reported as such when the field is decoded.
        KdlValue::Integer(number) => i64::try_from(*number)
            .map(Scalar::Int)
            .unwrap_or_else(|_| Scalar::Float(*number as f64)),
        KdlValue::Float(number) => Scalar::Float(*number),
        KdlValue::Bool(flag) => Scalar::Bool(*flag),
        KdlValue::Null => Scalar::Null,
    }
}

/// How deep a configuration may nest.
///
/// A real one nests about five: `site` > `route` > `headers` > `response`.
/// Sixteen is far beyond anything anybody writes and far below the depth at
/// which the parser runs out of stack, which is where the number has to sit —
/// it is a guard on the machine, not a limit on expression.
pub const MAX_NESTING: usize = 16;

// The dependency also recursively parses successive fragments inside one
// block comment, not just nested comments. Bound total fragments conservatively.
const MAX_COMMENT_STEPS: usize = 64;

/// The offset at which a known recursive syntax budget is exceeded.
///
/// This is a conservative pre-parser guard, not a complete KDL grammar. Keep
/// its string/comment/newline rules aligned with the locked parser dependency;
/// tests cover known recursive paths, not a proof that no other path exists.
fn too_deep(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut at = 0;
    let mut depth = 0usize;
    let mut disable_markers = 0usize;

    while at < bytes.len() {
        match bytes[at] {
            b'"' => {
                disable_markers = 0;
                at = skip_string(bytes, at, 0);
            }
            // `#"…"#` raw strings, and `#true` / `#null`, which are not.
            b'#' => {
                disable_markers = 0;
                let mut hashes = 0;
                while at + hashes < bytes.len() && bytes[at + hashes] == b'#' {
                    hashes += 1;
                }
                if bytes.get(at + hashes) == Some(&b'"') {
                    at = skip_string(bytes, at + hashes, hashes);
                } else {
                    at += hashes;
                }
            }
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                at += 2;
                while at < bytes.len() && newline_len(&bytes[at..]) == 0 {
                    at += 1;
                }
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                at = match skip_block_comment(bytes, at) {
                    Ok(end) => end,
                    Err(offset) => return Some(offset),
                };
            }
            b'/' if bytes.get(at + 1) == Some(&b'-') => {
                disable_markers += 1;
                if depth + disable_markers > MAX_NESTING {
                    return Some(at);
                }
                at += 2;
            }
            b'{' => {
                disable_markers = 0;
                depth += 1;
                if depth > MAX_NESTING {
                    return Some(at);
                }
                at += 1;
            }
            b'}' => {
                disable_markers = 0;
                depth = depth.saturating_sub(1);
                at += 1;
            }
            _ => {
                // Whitespace, comments and escaped newlines may separate /-
                // prefixes without unwinding the dependency's entry parser.
                let ch = text[at..].chars().next().unwrap();
                if !ch.is_whitespace() && !matches!(ch, '\\' | '\u{feff}') {
                    disable_markers = 0;
                }
                at += ch.len_utf8();
            }
        }
    }
    None
}

/// Move past a string that starts at `at`, which is its opening quote.
///
/// `hashes` is how many `#` preceded it, so a raw string ends at a quote
/// followed by the same number. An unterminated string runs to the end of the
/// file, which is right: the parser will refuse it, and it cannot contain a
/// brace that counts.
fn skip_string(bytes: &[u8], at: usize, hashes: usize) -> usize {
    // KDL's multi-line form. Checked first, or `"""` reads as an empty string
    // followed by a quote that swallows the rest of the file.
    let multiline = bytes[at..].starts_with(b"\"\"\"");
    let quotes = if multiline { 3 } else { 1 };
    let mut at = at + quotes;

    while at < bytes.len() {
        // Only a plain string has escapes; in a raw one a backslash is a
        // backslash.
        if hashes == 0 && bytes[at] == b'\\' {
            at += 2;
            continue;
        }
        if bytes[at] == b'"' && bytes[at..].len() >= quotes {
            let after = at + quotes;
            if bytes[at..after].iter().all(|byte| *byte == b'"')
                // Fail on the first mismatch. Counting hashes across the
                // entire suffix made every embedded quote scan `hashes`
                // bytes, even when the first following byte was not '#'.
                && bytes[after..]
                    .get(..hashes)
                    .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
            {
                return after + hashes;
            }
        }
        at += 1;
    }
    bytes.len()
}

/// Move past a block comment, which KDL allows to nest.
fn skip_block_comment(bytes: &[u8], at: usize) -> Result<usize, usize> {
    let mut at = at + 2;
    let mut open = 1usize;
    let mut steps = 0;
    while at < bytes.len() && open > 0 {
        steps += 1;
        if steps > MAX_COMMENT_STEPS {
            return Err(at);
        }
        if bytes[at..].starts_with(b"/*") {
            open += 1;
            if open > MAX_NESTING {
                return Err(at);
            }
            at += 2;
        } else if bytes[at..].starts_with(b"*/") {
            open -= 1;
            at += 2;
        } else if matches!(bytes[at], b'/' | b'*') {
            at += 1;
        } else {
            // The parser consumes a run of non-delimiter bytes in one step.
            while at < bytes.len() && !matches!(bytes[at], b'/' | b'*') {
                at += 1;
            }
        }
    }
    Ok(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_raw_delimiters_and_many_false_endings_keep_the_real_boundary() {
        let hashes = "#".repeat(2048);
        let source = format!("node {hashes}\"x{}\"{hashes}\n", "\"x".repeat(16_384));
        let start = std::time::Instant::now();
        assert_eq!(
            skip_string(source.as_bytes(), 5 + hashes.len(), hashes.len()),
            source.len() - 1
        );
        eprintln!("raw delimiter scanner fixture: {:?}", start.elapsed());
        let deep = format!(
            "{source}{}{}",
            "node {".repeat(MAX_NESTING + 1),
            "}".repeat(MAX_NESTING + 1)
        );
        assert_eq!(too_deep(&deep), Some(source.len() + 6 * MAX_NESTING + 5));
    }

    #[test]
    fn incomplete_raw_suffix_cannot_close_a_string() {
        for text in ["\"x\"##", "\"x\"#x#"] {
            assert_eq!(skip_string(text.as_bytes(), 0, 3), text.len());
        }
        let text = "\"x\"#x#\"###suffix";
        assert_eq!(skip_string(text.as_bytes(), 0, 3), text.len() - "suffix".len());
    }

    #[test]
    fn nested_disable_markers_are_rejected_before_recursive_parsing() {
        for separator in [" ", "\n", "\r", "\u{2003}", "/* note */", "// note\n", "\\\n"] {
            let source = format!("node {}1\n", format!("/-{separator}").repeat(MAX_NESTING + 1));
            assert!(too_deep(&source).is_some(), "separator {separator:?}");
            assert!(lower(&source, "disabled.kdl").is_err());
        }
    }

    #[test]
    fn independent_disabled_entries_and_marker_text_are_not_recursive_prefixes() {
        let source = format!("node {}\"active\"\n", "/- 1 ".repeat(100));
        assert!(lower(&source, "independent.kdl").is_ok());
        for source in [
            format!("node \"{}\"\n", "/- ".repeat(100)),
            format!("node #\"{}\"#\n", "/- ".repeat(100)),
            format!("// {}\nnode\n", "/- ".repeat(100)),
        ] {
            assert!(lower(&source, "text.kdl").is_ok());
        }
    }

    #[test]
    fn every_kdl_line_ending_stops_line_comments_before_depth_scanning() {
        for newline in [
            "\n", "\r\n", "\r", "\u{000b}", "\u{000c}", "\u{0085}", "\u{2028}", "\u{2029}",
        ] {
            let prefix = format!("// comment{newline}");
            let source = format!(
                "{prefix}{}{}",
                "node {".repeat(MAX_NESTING + 1),
                "}".repeat(MAX_NESTING + 1)
            );
            assert_eq!(
                too_deep(&source),
                Some(prefix.len() + 6 * MAX_NESTING + 5),
                "newline {newline:?}"
            );
            assert!(lower(&source, "deep.kdl").is_err());
            let valid = format!("{prefix}node \"value\"{newline}");
            let nodes = lower(&valid, "valid.kdl").unwrap();
            assert_eq!(nodes.len(), 1);
            let span = nodes[0].origin.span().unwrap();
            assert_eq!((span.line, span.column), (2, 1), "newline {newline:?}");
        }
    }

    #[test]
    fn deeply_nested_comments_are_rejected_by_the_preparse_guard() {
        let text = format!(
            "{}x{}\n",
            "/*".repeat(MAX_NESTING + 1),
            "*/".repeat(MAX_NESTING + 1)
        );
        // Exercise only the scanner before the fix: sending arbitrary depths
        // to the recursive dependency parser could abort the test process.
        assert_eq!(too_deep(&text), Some(2 * MAX_NESTING));
        assert!(lower(&text, "nested-comments.kdl").is_err());
    }

    #[test]
    fn flat_comment_fragments_are_bounded_but_long_plain_comments_are_allowed() {
        for fragment in ["*", "/", "a*b/"] {
            let text = format!("/* {} */\nnode\n", fragment.repeat(MAX_COMMENT_STEPS + 1));
            assert!(too_deep(&text).is_some());
            assert!(lower(&text, "comment.kdl").is_err());
        }
        for text in [
            format!("/* {} */\nnode\n", "ordinary text ".repeat(10_000)),
            format!(
                "{}x{}\nnode\n",
                "/*".repeat(MAX_NESTING),
                "*/".repeat(MAX_NESTING)
            ),
        ] {
            assert!(lower(&text, "comment.kdl").is_ok());
        }
    }

    #[test]
    fn positions_are_one_based_lines_and_columns() {
        let index = LineIndex::new("abc\ndef\n");
        assert_eq!(index.span(0, 1).line, 1);
        assert_eq!(index.span(0, 1).column, 1);
        assert_eq!(index.span(4, 1).line, 2);
        assert_eq!(index.span(6, 1).column, 3);
    }

    #[test]
    fn arguments_properties_and_children_survive_lowering() {
        let nodes = lower(
            r#"
site "example.com" listener="https" {
    route "/api/*" id="api" {
        respond "ok" status=201
    }
}
"#,
            "t.kdl",
        )
        .unwrap();
        assert_eq!(nodes.len(), 1);
        let site = &nodes[0];
        assert_eq!(site.name, "site");
        assert_eq!(site.args[0].as_str(), Some("example.com"));
        assert_eq!(site.prop("listener").unwrap().value.as_str(), Some("https"));
        let route = &site.children[0];
        assert_eq!(route.args[0].as_str(), Some("/api/*"));
        let respond = &route.children[0];
        assert_eq!(respond.prop("status").unwrap().value.as_int(), Some(201));
    }

    #[test]
    fn every_node_knows_where_it_came_from() {
        let nodes = lower("global {\n    http \":8080\"\n}\n", "gatel.kdl").unwrap();
        let http = &nodes[0].children[0];
        let span = http.origin.span().unwrap();
        assert_eq!(span.line, 2);
        assert_eq!(http.origin.file_name(), Some("gatel.kdl"));
    }

    #[test]
    fn booleans_and_nulls_lower_to_scalars() {
        let nodes = lower("import \"x.kdl\" optional=#true fallback=#null", "t.kdl").unwrap();
        assert_eq!(nodes[0].prop("optional").unwrap().value.as_bool(), Some(true));
        assert_eq!(nodes[0].prop("fallback").unwrap().value.scalar, Scalar::Null);
    }

    #[test]
    fn a_syntax_error_reports_a_position_not_a_panic() {
        let error = lower("site \"example.com\" {\n", "bad.kdl").unwrap_err();
        assert!(error.has_errors());
        let diagnostic = error.iter().next().unwrap();
        assert_eq!(diagnostic.code, Code::SyntaxError);
        assert_eq!(diagnostic.origin.file_name(), Some("bad.kdl"));
    }

    #[test]
    fn a_file_nested_past_the_limit_is_refused_before_it_is_parsed() {
        // The parser is recursive and runs out of stack somewhere past thirty
        // levels, which aborts the process rather than raising anything. This
        // has to be caught before the parser sees it, or nothing can catch it.
        let text = format!("{}{}", "a{".repeat(64), "}".repeat(64));
        let error = lower(&text, "deep.kdl").unwrap_err();
        let diagnostic = error.iter().next().unwrap();
        assert_eq!(diagnostic.code, Code::SyntaxError);
        assert!(
            diagnostic.message.contains("deeper than"),
            "{}",
            diagnostic.message
        );
        assert!(diagnostic.origin.span().is_some(), "it has to say where");
    }

    #[test]
    fn an_unclosed_brace_repeated_is_refused_the_same_way() {
        // The likeliest way anybody reaches this: an editor, a paste, a `{`
        // that was never closed.
        let error = lower(&"a{".repeat(200), "unclosed.kdl").unwrap_err();
        assert!(error.has_errors());
    }

    #[test]
    fn a_configuration_as_deep_as_a_real_one_is_fine() {
        let text = r#"
site "example.com" {
    route "/*" {
        headers {
            response {
                set "x-a" "b"
            }
        }
        respond "ok"
    }
}
"#;
        assert!(lower(text, "real.kdl").is_ok());
    }

    #[test]
    fn braces_inside_a_string_are_not_nesting() {
        // Otherwise an error page or a canned JSON body would count against a
        // limit it has nothing to do with.
        let deep = "{".repeat(64);
        let text = format!("site \"*\" {{ route \"/\" {{ respond \"{deep}\" }} }}");
        assert_eq!(too_deep(&text), None, "a string is not structure");
    }

    #[test]
    fn braces_inside_a_comment_are_not_nesting() {
        let deep = "{".repeat(64);
        assert_eq!(too_deep(&format!("// {deep}\nglobal {{ }}")), None);
        assert_eq!(too_deep(&format!("/* {deep} */\nglobal {{ }}")), None);
        // KDL block comments nest, so the inner one must not end the outer.
        assert_eq!(too_deep(&format!("/* /* {deep} */ */\nglobal {{ }}")), None);
    }

    #[test]
    fn braces_inside_a_raw_string_are_not_nesting() {
        let deep = "{".repeat(64);
        assert_eq!(too_deep(&format!("respond #\"{deep}\"#")), None);
        assert_eq!(too_deep(&format!("respond ##\"{deep}\"##")), None);
    }

    #[test]
    fn a_multiline_string_does_not_swallow_the_rest_of_the_file() {
        // `"""` read as an empty string followed by a quote would put the
        // scanner inside a string for ever, and it would count nothing at all.
        let deep = "a{".repeat(64);
        let text = format!("respond \"\"\"\nnothing\n\"\"\"\n{deep}");
        assert!(too_deep(&text).is_some(), "the tail still has to be counted");
    }

    #[test]
    fn a_hash_that_is_not_a_raw_string_is_just_a_hash() {
        // `#true` and `#null` are KDL's keywords.
        assert_eq!(too_deep("import \"x.kdl\" optional=#true"), None);
        assert!(too_deep(&format!("a=#true {}", "b{".repeat(64))).is_some());
    }

    #[test]
    fn an_escaped_quote_does_not_end_a_string() {
        let deep = "{".repeat(64);
        let text = format!("respond \"a\\\"{deep}\"");
        assert_eq!(too_deep(&text), None);
    }
}
