//! Configuration diagnostics.
//!
//! A diagnostic is a typed object, never a formatted string: the code is stable
//! across releases, the origin is machine readable, and the message is
//! guaranteed not to contain secret material.

use std::fmt;

use crate::origin::Origin;

/// Stable diagnostic codes. Adding a variant is a compatible change; changing
/// the string of an existing one is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Code {
    /// The source text could not be parsed at all.
    SyntaxError,
    /// A node name that no directive or resource claims.
    UnknownNode,
    /// A property name the target type does not accept.
    UnknownProperty,
    /// More or fewer positional arguments than the target type accepts.
    ArgumentCount,
    /// A value had the wrong shape (string where an integer was wanted, ...).
    InvalidValue,
    /// A required property or child node was absent.
    MissingField,
    /// The same key was declared twice where only one is allowed.
    Duplicate,
    /// A reference (upstream name, listener name, ...) did not resolve.
    UnresolvedReference,
    /// The node is syntactically fine but not allowed in this scope.
    InvalidScope,
    /// A route had zero or more than one terminal.
    TerminalArity,
    /// The build does not contain the capability this configuration needs.
    MissingCapability,
    /// An `import` could not be read, or resolved outside the config root.
    ImportFailed,
    /// A secret reference was malformed or pointed outside the config root.
    InvalidSecret,
    /// Two resources in different sources claimed the same identity.
    OwnershipConflict,
    /// Binding a socket, loading a certificate or starting a service failed.
    PrepareFailed,
}

impl Code {
    pub const fn as_str(self) -> &'static str {
        match self {
            Code::SyntaxError => "syntax",
            Code::UnknownNode => "unknown-node",
            Code::UnknownProperty => "unknown-property",
            Code::ArgumentCount => "argument-count",
            Code::InvalidValue => "invalid-value",
            Code::MissingField => "missing-field",
            Code::Duplicate => "duplicate",
            Code::UnresolvedReference => "unresolved-reference",
            Code::InvalidScope => "invalid-scope",
            Code::TerminalArity => "terminal-arity",
            Code::MissingCapability => "missing-capability",
            Code::ImportFailed => "import-failed",
            Code::InvalidSecret => "invalid-secret",
            Code::OwnershipConflict => "ownership-conflict",
            Code::PrepareFailed => "prepare-failed",
        }
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Warning,
    Error,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Severity::Warning => "warning",
            Severity::Error => "error",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: Code,
    pub severity: Severity,
    pub message: String,
    pub origin: Origin,
    /// An actionable suggestion. Rendered on its own line.
    pub help: Option<String>,
    /// A second location that explains the first, such as the earlier
    /// declaration a `Duplicate` conflicts with.
    pub related: Option<(String, Origin)>,
}

impl Diagnostic {
    pub fn error(code: Code, origin: Origin, message: impl Into<String>) -> Self {
        Self {
            code,
            severity: Severity::Error,
            message: message.into(),
            origin,
            help: None,
            related: None,
        }
    }

    pub fn warning(code: Code, origin: Origin, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            ..Self::error(code, origin, message)
        }
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    pub fn with_related(mut self, label: impl Into<String>, origin: Origin) -> Self {
        self.related = Some((label.into(), origin));
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}[{}]: {}\n  --> {}",
            self.severity, self.code, self.message, self.origin
        )?;
        if let Some((label, origin)) = &self.related {
            write!(f, "\n  --> {label}: {origin}")?;
        }
        if let Some(help) = &self.help {
            write!(f, "\n  help: {help}")?;
        }
        Ok(())
    }
}

/// An accumulator so a single pass can report every problem rather than
/// stopping at the first one.
#[derive(Debug, Clone, Default)]
pub struct Diagnostics {
    items: Vec<Diagnostic>,
}

impl Diagnostics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, diagnostic: Diagnostic) {
        self.items.push(diagnostic);
    }

    pub fn extend(&mut self, other: Diagnostics) {
        self.items.extend(other.items);
    }

    pub fn error(&mut self, code: Code, origin: Origin, message: impl Into<String>) {
        self.push(Diagnostic::error(code, origin, message));
    }

    pub fn has_errors(&self) -> bool {
        self.items.iter().any(Diagnostic::is_error)
    }

    pub fn error_count(&self) -> usize {
        self.items.iter().filter(|d| d.is_error()).count()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Diagnostic> {
        self.items.iter()
    }

    pub fn into_vec(self) -> Vec<Diagnostic> {
        self.items
    }

    /// Sort by source position so output is stable regardless of the order
    /// checks happened to run in.
    pub fn sorted(mut self) -> Self {
        self.items.sort_by(|a, b| {
            let key = |d: &Diagnostic| {
                (
                    d.origin.file_name().unwrap_or("").to_string(),
                    d.origin.span().map(|s| s.offset).unwrap_or(usize::MAX),
                    d.origin.path().to_string(),
                    d.code,
                )
            };
            key(a).cmp(&key(b))
        });
        self
    }
}

impl IntoIterator for Diagnostics {
    type Item = Diagnostic;
    type IntoIter = std::vec::IntoIter<Diagnostic>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter()
    }
}

impl FromIterator<Diagnostic> for Diagnostics {
    fn from_iter<T: IntoIterator<Item = Diagnostic>>(iter: T) -> Self {
        Self {
            items: iter.into_iter().collect(),
        }
    }
}

impl fmt::Display for Diagnostics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, item) in self.items.iter().enumerate() {
            if index > 0 {
                f.write_str("\n\n")?;
            }
            write!(f, "{item}")?;
        }
        Ok(())
    }
}

/// Suggest the closest candidate for a misspelled name, if one is close enough
/// to be worth showing.
pub fn did_you_mean<'a>(input: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let budget = match input.len() {
        0..=3 => 1,
        4..=7 => 2,
        _ => 3,
    };
    candidates
        .into_iter()
        .filter_map(|candidate| {
            let distance = edit_distance(input, candidate);
            (distance <= budget).then_some((distance, candidate))
        })
        .min_by_key(|(distance, candidate)| (*distance, candidate.len()))
        .map(|(_, candidate)| candidate)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b_chars.len()).collect();
    let mut current = vec![0usize; b_chars.len() + 1];
    for (i, a_char) in a.chars().enumerate() {
        current[0] = i + 1;
        for (j, b_char) in b_chars.iter().enumerate() {
            let cost = usize::from(a_char != *b_char);
            current[j + 1] = (previous[j] + cost).min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b_chars.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::origin::Span;

    fn origin() -> Origin {
        Origin::file("gatel.kdl", Span::new(0, 3, 2, 5))
    }

    #[test]
    fn display_includes_code_origin_and_help() {
        let rendered = Diagnostic::error(Code::UnknownNode, origin(), "unknown node `prox`")
            .with_help("did you mean `proxy`?")
            .to_string();
        assert!(rendered.contains("error[unknown-node]: unknown node `prox`"));
        assert!(rendered.contains("--> gatel.kdl:2:5"));
        assert!(rendered.contains("help: did you mean `proxy`?"));
    }

    #[test]
    fn diagnostics_sort_by_position() {
        let mut diagnostics = Diagnostics::new();
        diagnostics.error(
            Code::InvalidValue,
            Origin::file("a.kdl", Span::new(90, 1, 9, 1)),
            "b",
        );
        diagnostics.error(
            Code::InvalidValue,
            Origin::file("a.kdl", Span::new(10, 1, 2, 1)),
            "a",
        );
        let sorted = diagnostics.sorted().into_vec();
        assert_eq!(sorted[0].message, "a");
        assert_eq!(sorted[1].message, "b");
    }

    #[test]
    fn warnings_do_not_count_as_errors() {
        let mut diagnostics = Diagnostics::new();
        diagnostics.push(Diagnostic::warning(Code::Duplicate, origin(), "w"));
        assert!(!diagnostics.has_errors());
        diagnostics.error(Code::Duplicate, origin(), "e");
        assert!(diagnostics.has_errors());
        assert_eq!(diagnostics.error_count(), 1);
    }

    #[test]
    fn suggestions_respect_the_distance_budget() {
        let names = ["proxy", "respond", "headers", "file-server"];
        assert_eq!(did_you_mean("prox", names), Some("proxy"));
        assert_eq!(did_you_mean("header", names), Some("headers"));
        assert_eq!(did_you_mean("completely-different", names), None);
    }
}
