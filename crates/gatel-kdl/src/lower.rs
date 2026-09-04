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
}
