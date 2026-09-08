//! Syntax-only formatting; never expands imports or compiles plugin directives.
use std::path::Path;

use gatel_model::{Code, Diagnostics, Origin, Span};

pub struct Formatted {
    pub text: String,
    pub changed: bool,
}

/// Format one regular UTF-8 file with the loader's byte and nesting limits.
/// Comments are retained; the input file and imported files are never written.
pub fn format_file(path: &Path) -> Result<Formatted, Diagnostics> {
    let file = path.display().to_string();
    let original =
        crate::load::read_bounded(path, crate::Limits::default().max_bytes_per_file).map_err(|message| {
            let mut diagnostics = Diagnostics::new();
            diagnostics.error(
                Code::ImportFailed,
                Origin::file(file.clone(), Span::default()),
                message,
            );
            diagnostics
        })?;
    format_text(&original, &file)
}

fn format_text(original: &str, file: &str) -> Result<Formatted, Diagnostics> {
    let mut document = crate::lower::parse(original, file)?;
    let unformatted = document.clone();
    document.autoformat();
    restore_inline_format(&unformatted, &mut document);
    let text = document.to_string();
    Ok(Formatted {
        changed: text != original,
        text,
    })
}

// KDL's default formatter clears entry formatting (including /- disabled
// entries) and before-children comments. Preserve those syntax-bearing areas;
// document indentation and node terminators can still be normalized.
fn restore_inline_format(before: &kdl::KdlDocument, after: &mut kdl::KdlDocument) {
    for (before, after) in before.nodes().iter().zip(after.nodes_mut()) {
        for (before, after) in before.entries().iter().zip(after.entries_mut()) {
            if let Some(format) = before.format() {
                after.set_format(format.clone());
            }
        }
        if let (Some(before), Some(after)) = (before.format(), after.format_mut()) {
            if !before.before_children.trim().is_empty() {
                after.before_children.clone_from(&before.before_children);
            }
            if !before.before_terminator.trim().is_empty() {
                after.before_terminator.clone_from(&before.before_terminator);
            }
            if before.terminator.contains('/') {
                after.terminator.clone_from(&before.terminator);
            }
            if !before.trailing.trim().is_empty() {
                after.trailing.clone_from(&before.trailing);
            }
        }
        if let (Some(before), Some(after)) = (before.children(), after.children_mut()) {
            restore_inline_format(before, after);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting_preserves_active_syntax_and_is_idempotent() {
        for source in [
            "",
            "   \n\t\n",
            "// comment only\n",
            "/* outer /* inner */ end */\n",
            "node /-secret=\"disabled\" enabled=#true\n/-off { ignored 1; }\n",
            "node /- { ignored 1; }\n",
            "node /* before children */ { child; }\n",
            "node /* before terminator */;\n",
            "node ##\"raw \\\" { } # string\"##\n",
            "node \"\"\"\n    first\n      second\n    \"\"\"\n",
            "node #\"\"\"\n    raw \\n { }\n    \"\"\"#\n",
            "(kind)node (unit)0x10 flag=#false empty=#null\n",
            "node 170141183460469231731687303715884105727 -0.0 #nan #inf #-inf\n",
            "node \\\n    \"continued\" // trailing\n",
        ] {
            let formatted = format_text(source, "fixture").unwrap();
            for retained in [
                "comment only",
                "outer",
                "inner",
                "disabled",
                "ignored",
                "trailing",
                "before children",
                "before terminator",
            ] {
                if source.contains(retained) {
                    assert!(
                        formatted.text.contains(retained),
                        "lost {retained}: {source:?} -> {:?}",
                        formatted.text
                    );
                }
            }
            let again = format_text(&formatted.text, "fixture").unwrap();
            assert!(
                !again.changed,
                "non-idempotent: {source:?} -> {:?} -> {:?}",
                formatted.text, again.text
            );
            let mut before = crate::lower::parse(source, "fixture").unwrap();
            let mut after = crate::lower::parse(&formatted.text, "fixture").unwrap();
            before.clear_format_recursive();
            after.clear_format_recursive();
            assert_eq!(before, after, "changed active syntax: {source:?}");
            // KDL's own equality normalizes NaN/infinity/signed zero. Verify
            // exact float bits separately rather than relying on that equality.
            for (left, right) in before.nodes().iter().zip(after.nodes()) {
                for (left, right) in left.entries().iter().zip(right.entries()) {
                    if let (kdl::KdlValue::Float(left), kdl::KdlValue::Float(right)) =
                        (left.value(), right.value())
                    {
                        assert_eq!(left.to_bits(), right.to_bits());
                    }
                }
            }
        }
    }
}
