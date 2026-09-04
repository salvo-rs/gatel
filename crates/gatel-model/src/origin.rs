//! Where a piece of configuration came from.
//!
//! Every frontend (KDL file, Admin JSON, Kubernetes object) produces the same
//! [`Origin`] shape so diagnostics never need to know which frontend they came
//! from.

use std::fmt;
use std::sync::Arc;

/// A byte span inside a source text, with a pre-computed line/column so
/// rendering a diagnostic never needs the original bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub offset: usize,
    pub length: usize,
    pub line: u32,
    pub column: u32,
}

impl Span {
    pub const fn new(offset: usize, length: usize, line: u32, column: u32) -> Self {
        Self {
            offset,
            length,
            line,
            column,
        }
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.column)
    }
}

/// The location a value or resource was declared at.
///
/// `path` is the dotted field path within the declaring node (for example
/// `upstream.timeouts.connect`), which is what makes a diagnostic actionable
/// once the frontend-specific part is stripped away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    kind: OriginKind,
    path: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OriginKind {
    /// A file-backed frontend such as KDL.
    File { file: Arc<str>, span: Span },
    /// A structured API frontend such as the Admin REST API.
    Api { source: Arc<str>, pointer: Arc<str> },
    /// Produced by Gatel itself (defaults, derived resources).
    Synthetic { reason: Arc<str> },
}

impl Origin {
    pub fn file(file: impl Into<Arc<str>>, span: Span) -> Self {
        Self {
            kind: OriginKind::File {
                file: file.into(),
                span,
            },
            path: Arc::from(""),
        }
    }

    pub fn api(source: impl Into<Arc<str>>, pointer: impl Into<Arc<str>>) -> Self {
        Self {
            kind: OriginKind::Api {
                source: source.into(),
                pointer: pointer.into(),
            },
            path: Arc::from(""),
        }
    }

    pub fn synthetic(reason: impl Into<Arc<str>>) -> Self {
        Self {
            kind: OriginKind::Synthetic {
                reason: reason.into(),
            },
            path: Arc::from(""),
        }
    }

    /// Append a field name to the dotted path, returning a new origin.
    pub fn field(&self, name: &str) -> Self {
        let path = if self.path.is_empty() {
            Arc::from(name)
        } else {
            Arc::from(format!("{}.{}", self.path, name).as_str())
        };
        Self {
            kind: self.kind.clone(),
            path,
        }
    }

    /// Replace the location while keeping the accumulated field path.
    pub fn at(&self, span: Span) -> Self {
        match &self.kind {
            OriginKind::File { file, .. } => Self {
                kind: OriginKind::File {
                    file: Arc::clone(file),
                    span,
                },
                path: Arc::clone(&self.path),
            },
            _ => self.clone(),
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn file_name(&self) -> Option<&str> {
        match &self.kind {
            OriginKind::File { file, .. } => Some(file),
            _ => None,
        }
    }

    pub fn span(&self) -> Option<Span> {
        match &self.kind {
            OriginKind::File { span, .. } => Some(*span),
            _ => None,
        }
    }

    pub fn is_synthetic(&self) -> bool {
        matches!(self.kind, OriginKind::Synthetic { .. })
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            OriginKind::File { file, span } => write!(f, "{file}:{span}")?,
            OriginKind::Api { source, pointer } => write!(f, "{source}{pointer}")?,
            OriginKind::Synthetic { reason } => write!(f, "<{reason}>")?,
        }
        if !self.path.is_empty() {
            write!(f, " ({})", self.path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_paths_accumulate_and_render() {
        let origin = Origin::file("gatel.kdl", Span::new(0, 4, 3, 5));
        let nested = origin.field("upstream").field("timeouts").field("connect");
        assert_eq!(nested.path(), "upstream.timeouts.connect");
        assert_eq!(nested.to_string(), "gatel.kdl:3:5 (upstream.timeouts.connect)");
    }

    #[test]
    fn at_keeps_path_and_moves_span() {
        let origin = Origin::file("a.kdl", Span::new(0, 1, 1, 1)).field("site");
        let moved = origin.at(Span::new(9, 2, 4, 7));
        assert_eq!(moved.path(), "site");
        assert_eq!(moved.span().unwrap().line, 4);
    }

    #[test]
    fn api_origin_renders_pointer() {
        let origin = Origin::api("admin", "/routes/0").field("proxy");
        assert_eq!(origin.to_string(), "admin/routes/0 (proxy)");
        assert!(origin.span().is_none());
    }
}
