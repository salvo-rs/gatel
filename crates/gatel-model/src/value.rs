//! The format-neutral configuration tree.
//!
//! Every frontend lowers its own syntax into [`Node`]s. Directives and
//! resources only ever see this tree, so one `#[derive(Directive)]` decoder
//! serves the KDL file frontend, the Admin API and Kubernetes alike.

use std::fmt;

use crate::origin::Origin;

/// A scalar configuration value.
///
/// Durations and sizes are deliberately absent: they arrive as strings or
/// integers on every frontend and become typed at decode time, which keeps the
/// tree itself a faithful, lossless view of the input.
#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

impl Scalar {
    pub fn type_name(&self) -> &'static str {
        match self {
            Scalar::Null => "null",
            Scalar::Bool(_) => "boolean",
            Scalar::Int(_) => "integer",
            Scalar::Float(_) => "float",
            Scalar::Str(_) => "string",
        }
    }
}

impl fmt::Display for Scalar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scalar::Null => f.write_str("null"),
            Scalar::Bool(value) => write!(f, "{value}"),
            Scalar::Int(value) => write!(f, "{value}"),
            Scalar::Float(value) => write!(f, "{value}"),
            Scalar::Str(value) => write!(f, "{value}"),
        }
    }
}

/// A scalar together with where it was written.
#[derive(Debug, Clone, PartialEq)]
pub struct Value {
    pub scalar: Scalar,
    pub origin: Origin,
}

impl Value {
    pub fn new(scalar: Scalar, origin: Origin) -> Self {
        Self { scalar, origin }
    }

    pub fn string(value: impl Into<String>, origin: Origin) -> Self {
        Self::new(Scalar::Str(value.into()), origin)
    }

    pub fn int(value: i64, origin: Origin) -> Self {
        Self::new(Scalar::Int(value), origin)
    }

    pub fn bool(value: bool, origin: Origin) -> Self {
        Self::new(Scalar::Bool(value), origin)
    }

    pub fn as_str(&self) -> Option<&str> {
        match &self.scalar {
            Scalar::Str(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self.scalar {
            Scalar::Int(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self.scalar {
            Scalar::Bool(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self.scalar {
            Scalar::Float(value) => Some(value),
            Scalar::Int(value) => Some(value as f64),
            _ => None,
        }
    }

    pub fn type_name(&self) -> &'static str {
        self.scalar.type_name()
    }
}

/// A named property on a node.
#[derive(Debug, Clone, PartialEq)]
pub struct Property {
    pub name: String,
    pub value: Value,
    /// Location of the property name, used when reporting an unknown key.
    pub origin: Origin,
}

/// One configuration node: a name, positional arguments, named properties and
/// child nodes. This mirrors KDL's shape because KDL is the primary frontend,
/// but JSON objects and Kubernetes resources map onto it without loss.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub name: String,
    pub args: Vec<Value>,
    pub props: Vec<Property>,
    pub children: Vec<Node>,
    pub origin: Origin,
}

impl Node {
    pub fn new(name: impl Into<String>, origin: Origin) -> Self {
        Self {
            name: name.into(),
            args: Vec::new(),
            props: Vec::new(),
            children: Vec::new(),
            origin,
        }
    }

    pub fn with_arg(mut self, value: Value) -> Self {
        self.args.push(value);
        self
    }

    pub fn with_prop(mut self, name: impl Into<String>, value: Value) -> Self {
        let origin = value.origin.clone();
        self.props.push(Property {
            name: name.into(),
            value,
            origin,
        });
        self
    }

    pub fn with_child(mut self, child: Node) -> Self {
        self.children.push(child);
        self
    }

    pub fn prop(&self, name: &str) -> Option<&Property> {
        self.props.iter().find(|property| property.name == name)
    }

    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|child| child.name == name)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |child| child.name == name)
    }

    /// Total node count including this one, used to bound pathological inputs.
    pub fn node_count(&self) -> usize {
        1 + self.children.iter().map(Node::node_count).sum::<usize>()
    }

    pub fn depth(&self) -> usize {
        1 + self.children.iter().map(Node::depth).max().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::origin::Span;

    fn origin() -> Origin {
        Origin::file("t.kdl", Span::default())
    }

    #[test]
    fn builder_produces_the_expected_shape() {
        let node = Node::new("route", origin())
            .with_arg(Value::string("/api/*", origin()))
            .with_prop("id", Value::string("api", origin()))
            .with_child(Node::new("proxy", origin()));
        assert_eq!(node.args[0].as_str(), Some("/api/*"));
        assert_eq!(node.prop("id").unwrap().value.as_str(), Some("api"));
        assert!(node.child("proxy").is_some());
        assert!(node.child("respond").is_none());
    }

    #[test]
    fn int_widens_to_float_but_not_the_reverse() {
        assert_eq!(Value::int(3, origin()).as_f64(), Some(3.0));
        assert_eq!(Value::new(Scalar::Float(1.5), origin()).as_int(), None);
    }

    #[test]
    fn counts_and_depth_walk_the_whole_tree() {
        let node = Node::new("site", origin())
            .with_child(Node::new("route", origin()).with_child(Node::new("proxy", origin())))
            .with_child(Node::new("route", origin()));
        assert_eq!(node.node_count(), 4);
        assert_eq!(node.depth(), 3);
        assert_eq!(node.children_named("route").count(), 2);
    }
}
