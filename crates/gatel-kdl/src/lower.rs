//! Lowering KDL syntax into the format-neutral node tree.
//!
//! This is the only place in Gatel that knows KDL exists. Everything after it
//! sees [`gatel_model::Node`], which is why the Admin API and a future
//! Kubernetes frontend reuse every decoder unchanged.

use gatel_model::{Code, Diagnostic, Diagnostics, Node, Origin, Property, Scalar, Span, Value};
use kdl::{KdlDocument, KdlNode, KdlValue};

/// Maps byte offsets to line and column, computed once per file.
pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(index, _)| index + 1));
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
                format!("nesting is deeper than {MAX_NESTING} levels"),
            )
            .with_help("a real configuration nests about five deep; this is usually an unclosed `{`"),
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
    Ok(document
        .nodes()
        .iter()
        .map(|node| lower_node(node, file, &index))
        .collect())
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

/// The offset of the brace that goes past [`MAX_NESTING`], if there is one.
///
/// A scanner rather than a parser: it only has to know where a `{` is *not* a
/// brace, which means strings and comments. Anything it gets wrong it gets
/// wrong by counting too much, so it can refuse a file the parser would have
/// survived — never the reverse.
fn too_deep(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut at = 0;
    let mut depth = 0usize;

    while at < bytes.len() {
        match bytes[at] {
            b'"' => at = skip_string(bytes, at, 0),
            // `#"…"#` raw strings, and `#true` / `#null`, which are not.
            b'#' => {
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
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => at = skip_block_comment(bytes, at),
            b'{' => {
                depth += 1;
                if depth > MAX_NESTING {
                    return Some(at);
                }
                at += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                at += 1;
            }
            _ => at += 1,
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
                && bytes[after..]
                    .iter()
                    .take(hashes)
                    .filter(|byte| **byte == b'#')
                    .count()
                    == hashes
            {
                return after + hashes;
            }
        }
        at += 1;
    }
    bytes.len()
}

/// Move past a block comment, which KDL allows to nest.
fn skip_block_comment(bytes: &[u8], at: usize) -> usize {
    let mut at = at + 2;
    let mut open = 1usize;
    while at < bytes.len() && open > 0 {
        if bytes[at..].starts_with(b"/*") {
            open += 1;
            at += 2;
        } else if bytes[at..].starts_with(b"*/") {
            open -= 1;
            at += 2;
        } else {
            at += 1;
        }
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;

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
