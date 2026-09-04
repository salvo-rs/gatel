//! Turning the format-neutral tree into typed Rust values.
//!
//! [`NodeReader`] is what `#[derive(Directive)]` and `#[derive(Resource)]`
//! generate calls into. It tracks which arguments, properties and children were
//! consumed so [`NodeReader::finish`] can reject anything left over — that is
//! how "unknown node or property is an error" is enforced in exactly one place.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::time::Duration;

use crate::diagnostic::{Code, Diagnostics, did_you_mean};
use crate::origin::Origin;
use crate::value::{Node, Value};

/// A byte quantity written as `16KiB`, `1MiB`, `1GB` or a plain integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ByteSize(pub u64);

impl ByteSize {
    pub const fn bytes(self) -> u64 {
        self.0
    }

    pub const fn kib(value: u64) -> Self {
        Self(value * 1024)
    }

    pub const fn mib(value: u64) -> Self {
        Self(value * 1024 * 1024)
    }
}

impl std::fmt::Display for ByteSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const UNITS: [(u64, &str); 3] = [(1024 * 1024 * 1024, "GiB"), (1024 * 1024, "MiB"), (1024, "KiB")];
        for (factor, suffix) in UNITS {
            if self.0 >= factor && self.0.is_multiple_of(factor) {
                return write!(f, "{}{suffix}", self.0 / factor);
            }
        }
        write!(f, "{}", self.0)
    }
}

/// Parse `500ms`, `2s`, `5m`, `1h`, `2d`. Bare integers are rejected on
/// purpose: a unit-less duration reads ambiguously in configuration.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let trimmed = text.trim();
    let split = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("`{trimmed}` needs a unit, for example `{trimmed}s`"))?;
    if split == 0 {
        return Err(format!("`{trimmed}` does not start with a number"));
    }
    let (number, unit) = trimmed.split_at(split);
    let value: u64 = number
        .parse()
        .map_err(|_| format!("`{number}` is not a whole number"))?;
    let millis = match unit {
        "ms" => Some(value),
        "s" => value.checked_mul(1_000),
        "m" => value.checked_mul(60_000),
        "h" => value.checked_mul(3_600_000),
        "d" => value.checked_mul(86_400_000),
        other => {
            return Err(format!(
                "unknown duration unit `{other}`, expected ms, s, m, h or d"
            ));
        }
    }
    .ok_or_else(|| format!("duration `{trimmed}` is too large"))?;
    Ok(Duration::from_millis(millis))
}

/// Parse `16KiB`, `1MiB`, `2GiB`, decimal `KB`/`MB`/`GB`, or a plain integer.
pub fn parse_size(text: &str) -> Result<ByteSize, String> {
    let trimmed = text.trim();
    let split = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    if split == 0 {
        return Err(format!("`{trimmed}` does not start with a number"));
    }
    let (number, unit) = trimmed.split_at(split);
    let value: u64 = number
        .parse()
        .map_err(|_| format!("`{number}` is not a whole number"))?;
    let factor = match unit.trim() {
        "" | "B" => 1,
        "KiB" => 1024,
        "MiB" => 1024 * 1024,
        "GiB" => 1024 * 1024 * 1024,
        "KB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        other => {
            return Err(format!(
                "unknown size unit `{other}`, expected B, KiB, MiB, GiB, KB, MB or GB"
            ));
        }
    };
    value
        .checked_mul(factor)
        .map(ByteSize)
        .ok_or_else(|| format!("size `{trimmed}` is too large"))
}

/// A scalar that can be produced from a configuration value.
pub trait FromValue: Sized {
    /// Human readable name of the expected shape, used in diagnostics.
    fn type_hint() -> &'static str;
    fn from_value(value: &Value, diagnostics: &mut Diagnostics) -> Option<Self>;
}

/// A structure that can be produced from a whole node.
pub trait FromNode: Sized {
    fn from_node(node: &Node, diagnostics: &mut Diagnostics) -> Option<Self>;
}

fn wrong_type<T: FromValue>(value: &Value, diagnostics: &mut Diagnostics) -> Option<T> {
    diagnostics.error(
        Code::InvalidValue,
        value.origin.clone(),
        format!("expected {}, found {}", T::type_hint(), value.type_name()),
    );
    None
}

impl FromValue for String {
    fn type_hint() -> &'static str {
        "a string"
    }

    fn from_value(value: &Value, diagnostics: &mut Diagnostics) -> Option<Self> {
        match value.as_str() {
            Some(text) => Some(text.to_string()),
            None => wrong_type::<Self>(value, diagnostics),
        }
    }
}

impl FromValue for bool {
    fn type_hint() -> &'static str {
        "a boolean"
    }

    fn from_value(value: &Value, diagnostics: &mut Diagnostics) -> Option<Self> {
        match value.as_bool() {
            Some(flag) => Some(flag),
            None => wrong_type::<Self>(value, diagnostics),
        }
    }
}

impl FromValue for f64 {
    fn type_hint() -> &'static str {
        "a number"
    }

    fn from_value(value: &Value, diagnostics: &mut Diagnostics) -> Option<Self> {
        match value.as_f64() {
            Some(number) => Some(number),
            None => wrong_type::<Self>(value, diagnostics),
        }
    }
}

macro_rules! impl_from_value_int {
    ($($ty:ty),*) => {$(
        impl FromValue for $ty {
            fn type_hint() -> &'static str {
                concat!("an integer in the range of ", stringify!($ty))
            }

            fn from_value(value: &Value, diagnostics: &mut Diagnostics) -> Option<Self> {
                let Some(raw) = value.as_int() else {
                    return wrong_type::<Self>(value, diagnostics);
                };
                match <$ty>::try_from(raw) {
                    Ok(converted) => Some(converted),
                    Err(_) => {
                        diagnostics.error(
                            Code::InvalidValue,
                            value.origin.clone(),
                            format!("{raw} is out of range for {}", stringify!($ty)),
                        );
                        None
                    }
                }
            }
        }
    )*};
}

impl_from_value_int!(i64, i32, u8, u16, u32, u64, usize);

impl FromValue for Duration {
    fn type_hint() -> &'static str {
        "a duration such as `500ms`, `30s`, `5m`, `1h` or `1d`"
    }

    fn from_value(value: &Value, diagnostics: &mut Diagnostics) -> Option<Self> {
        let Some(text) = value.as_str() else {
            return wrong_type::<Self>(value, diagnostics);
        };
        match parse_duration(text) {
            Ok(duration) => Some(duration),
            Err(message) => {
                diagnostics.error(Code::InvalidValue, value.origin.clone(), message);
                None
            }
        }
    }
}

impl FromValue for ByteSize {
    fn type_hint() -> &'static str {
        "a size such as `16KiB` or `1MiB`"
    }

    fn from_value(value: &Value, diagnostics: &mut Diagnostics) -> Option<Self> {
        let text = match &value.scalar {
            crate::value::Scalar::Str(text) => text.clone(),
            crate::value::Scalar::Int(number) if *number >= 0 => number.to_string(),
            _ => return wrong_type::<Self>(value, diagnostics),
        };
        match parse_size(&text) {
            Ok(size) => Some(size),
            Err(message) => {
                diagnostics.error(Code::InvalidValue, value.origin.clone(), message);
                None
            }
        }
    }
}

impl FromValue for SocketAddr {
    fn type_hint() -> &'static str {
        "a numeric socket address such as `127.0.0.1:8080`"
    }

    fn from_value(value: &Value, diagnostics: &mut Diagnostics) -> Option<Self> {
        let Some(text) = value.as_str() else {
            return wrong_type::<Self>(value, diagnostics);
        };
        match parse_socket_addr(text) {
            Ok(addr) => Some(addr),
            Err(message) => {
                diagnostics.error(Code::InvalidValue, value.origin.clone(), message);
                None
            }
        }
    }
}

/// Accept `127.0.0.1:8080`, `[::1]:8080` and the `:8080` shorthand for
/// `0.0.0.0:8080`.
pub fn parse_socket_addr(text: &str) -> Result<SocketAddr, String> {
    let candidate = if let Some(port) = text.strip_prefix(':') {
        format!("0.0.0.0:{port}")
    } else {
        text.to_string()
    };
    let addr: SocketAddr = candidate
        .parse()
        .map_err(|_| format!("`{text}` is not a numeric `address:port`"))?;
    if addr.port() == 0 {
        return Err(format!("`{text}` must use a non-zero port"));
    }
    Ok(addr)
}

/// Consumes a node's parts, tracking what was used so leftovers can be
/// reported as errors.
pub struct NodeReader<'a> {
    node: &'a Node,
    diagnostics: &'a mut Diagnostics,
    next_arg: usize,
    known_props: BTreeSet<&'static str>,
    known_children: BTreeSet<&'static str>,
    max_args: Option<usize>,
    consumed_children: bool,
}

impl<'a> NodeReader<'a> {
    pub fn new(node: &'a Node, diagnostics: &'a mut Diagnostics) -> Self {
        Self {
            node,
            diagnostics,
            next_arg: 0,
            known_props: BTreeSet::new(),
            known_children: BTreeSet::new(),
            max_args: None,
            consumed_children: false,
        }
    }

    pub fn node(&self) -> &'a Node {
        self.node
    }

    pub fn origin(&self) -> &Origin {
        &self.node.origin
    }

    pub fn diagnostics(&mut self) -> &mut Diagnostics {
        self.diagnostics
    }

    /// A required positional argument.
    pub fn arg<T: FromValue>(&mut self, name: &str) -> Option<T> {
        match self.node.args.get(self.next_arg) {
            Some(value) => {
                self.next_arg += 1;
                self.max_args = Some(self.next_arg);
                T::from_value(value, self.diagnostics)
            }
            None => {
                self.max_args = Some(self.next_arg + 1);
                self.diagnostics.push(crate::diagnostic::Diagnostic::error(
                    Code::ArgumentCount,
                    self.node.origin.field(name),
                    format!("`{}` requires {name} as {}", self.node.name, T::type_hint()),
                ));
                None
            }
        }
    }

    /// An optional positional argument.
    pub fn opt_arg<T: FromValue>(&mut self, _name: &str) -> Option<T> {
        let value = self.node.args.get(self.next_arg)?;
        self.next_arg += 1;
        self.max_args = Some(self.next_arg);
        T::from_value(value, self.diagnostics)
    }

    /// Every remaining positional argument.
    pub fn rest_args<T: FromValue>(&mut self, _name: &str) -> Vec<T> {
        let mut collected = Vec::new();
        while let Some(value) = self.node.args.get(self.next_arg) {
            self.next_arg += 1;
            if let Some(item) = T::from_value(value, self.diagnostics) {
                collected.push(item);
            }
        }
        self.max_args = None;
        collected
    }

    /// A required named property.
    pub fn prop<T: FromValue>(&mut self, key: &'static str) -> Option<T> {
        self.known_props.insert(key);
        match self.node.prop(key) {
            Some(property) => T::from_value(&property.value, self.diagnostics),
            None => {
                self.diagnostics.push(crate::diagnostic::Diagnostic::error(
                    Code::MissingField,
                    self.node.origin.field(key),
                    format!("`{}` requires the `{key}` property", self.node.name),
                ));
                None
            }
        }
    }

    /// An optional named property.
    pub fn opt_prop<T: FromValue>(&mut self, key: &'static str) -> Option<T> {
        self.known_props.insert(key);
        let property = self.node.prop(key)?;
        T::from_value(&property.value, self.diagnostics)
    }

    /// A required child node decoded as `T`.
    pub fn child<T: FromNode>(&mut self, name: &'static str) -> Option<T> {
        self.known_children.insert(name);
        match self.node.child(name) {
            Some(child) => T::from_node(child, self.diagnostics),
            None => {
                self.diagnostics.push(crate::diagnostic::Diagnostic::error(
                    Code::MissingField,
                    self.node.origin.field(name),
                    format!("`{}` requires a `{name}` child node", self.node.name),
                ));
                None
            }
        }
    }

    /// An optional child node, rejecting a second declaration.
    pub fn opt_child<T: FromNode>(&mut self, name: &'static str) -> Option<T> {
        self.known_children.insert(name);
        let mut matching = self.node.children_named(name);
        let first = matching.next()?;
        if let Some(second) = matching.next() {
            self.diagnostics.push(
                crate::diagnostic::Diagnostic::error(
                    Code::Duplicate,
                    second.origin.clone(),
                    format!("`{name}` may only be declared once in `{}`", self.node.name),
                )
                .with_related("first declared here", first.origin.clone()),
            );
        }
        T::from_node(first, self.diagnostics)
    }

    /// Every child node with this name.
    pub fn children<T: FromNode>(&mut self, name: &'static str) -> Vec<T> {
        self.known_children.insert(name);
        self.node
            .children_named(name)
            .filter_map(|child| T::from_node(child, self.diagnostics))
            .collect()
    }

    /// Declare that child nodes are handled elsewhere (a route's directives,
    /// for example), so `finish` must not report them as unknown.
    pub fn takes_any_children(&mut self) {
        self.consumed_children = true;
    }

    /// Report leftover arguments, properties and children.
    pub fn finish(self) {
        let NodeReader {
            node,
            diagnostics,
            next_arg,
            known_props,
            known_children,
            max_args,
            consumed_children,
        } = self;

        if let Some(max) = max_args
            && node.args.len() > max
            && let Some(extra) = node.args.get(max)
        {
            diagnostics.error(
                Code::ArgumentCount,
                extra.origin.clone(),
                format!(
                    "`{}` accepts {max} argument{}, found {}",
                    node.name,
                    if max == 1 { "" } else { "s" },
                    node.args.len()
                ),
            );
        }
        let _ = next_arg;

        for property in &node.props {
            if known_props.contains(property.name.as_str()) {
                continue;
            }
            let mut diagnostic = crate::diagnostic::Diagnostic::error(
                Code::UnknownProperty,
                property.origin.clone(),
                format!("`{}` has no property `{}`", node.name, property.name),
            );
            if let Some(suggestion) = did_you_mean(&property.name, known_props.iter().copied()) {
                diagnostic = diagnostic.with_help(format!("did you mean `{suggestion}`?"));
            } else if !known_props.is_empty() {
                diagnostic = diagnostic.with_help(format!(
                    "known properties: {}",
                    known_props.iter().copied().collect::<Vec<_>>().join(", ")
                ));
            }
            diagnostics.push(diagnostic);
        }

        if consumed_children {
            return;
        }
        for child in &node.children {
            if known_children.contains(child.name.as_str()) {
                continue;
            }
            let mut diagnostic = crate::diagnostic::Diagnostic::error(
                Code::UnknownNode,
                child.origin.clone(),
                format!("`{}` has no child node `{}`", node.name, child.name),
            );
            if let Some(suggestion) = did_you_mean(&child.name, known_children.iter().copied()) {
                diagnostic = diagnostic.with_help(format!("did you mean `{suggestion}`?"));
            } else if !known_children.is_empty() {
                diagnostic = diagnostic.with_help(format!(
                    "known child nodes: {}",
                    known_children.iter().copied().collect::<Vec<_>>().join(", ")
                ));
            }
            diagnostics.push(diagnostic);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::origin::Span;
    use crate::value::{Scalar, Value};

    fn origin() -> Origin {
        Origin::file("t.kdl", Span::default())
    }

    #[test]
    fn durations_cover_every_unit_and_reject_bare_numbers() {
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("2s").unwrap(), Duration::from_secs(2));
        assert_eq!(parse_duration("3m").unwrap(), Duration::from_secs(180));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86_400));
        assert!(parse_duration("30").is_err());
        assert!(parse_duration("5w").is_err());
    }

    #[test]
    fn sizes_accept_binary_decimal_and_plain_integers() {
        assert_eq!(parse_size("16KiB").unwrap(), ByteSize(16 * 1024));
        assert_eq!(parse_size("1MB").unwrap(), ByteSize(1_000_000));
        assert_eq!(parse_size("512").unwrap(), ByteSize(512));
        assert!(parse_size("1TiB").is_err());
        assert_eq!(ByteSize::mib(16).to_string(), "16MiB");
    }

    #[test]
    fn socket_addresses_expand_the_port_shorthand() {
        assert_eq!(parse_socket_addr(":8080").unwrap().to_string(), "0.0.0.0:8080");
        assert!(parse_socket_addr("127.0.0.1:0").is_err());
        assert!(parse_socket_addr("example.com:80").is_err());
    }

    #[test]
    fn unknown_property_is_an_error_with_a_suggestion() {
        let node = Node::new("respond", origin()).with_prop("statuss", Value::int(404, origin()));
        let mut diagnostics = Diagnostics::new();
        let mut reader = NodeReader::new(&node, &mut diagnostics);
        let _: Option<u16> = reader.opt_prop("status");
        reader.finish();
        assert_eq!(diagnostics.error_count(), 1);
        let diagnostic = diagnostics.iter().next().unwrap();
        assert_eq!(diagnostic.code, Code::UnknownProperty);
        assert_eq!(diagnostic.help.as_deref(), Some("did you mean `status`?"));
    }

    #[test]
    fn extra_positional_arguments_are_rejected() {
        let node = Node::new("respond", origin())
            .with_arg(Value::string("a", origin()))
            .with_arg(Value::string("b", origin()));
        let mut diagnostics = Diagnostics::new();
        let mut reader = NodeReader::new(&node, &mut diagnostics);
        let _: Option<String> = reader.opt_arg("body");
        reader.finish();
        assert_eq!(diagnostics.error_count(), 1);
        assert_eq!(diagnostics.iter().next().unwrap().code, Code::ArgumentCount);
    }

    #[test]
    fn rest_args_absorb_everything_left() {
        let node = Node::new("method", origin())
            .with_arg(Value::string("GET", origin()))
            .with_arg(Value::string("POST", origin()));
        let mut diagnostics = Diagnostics::new();
        let mut reader = NodeReader::new(&node, &mut diagnostics);
        let methods: Vec<String> = reader.rest_args("method");
        reader.finish();
        assert_eq!(methods, vec!["GET", "POST"]);
        assert!(!diagnostics.has_errors());
    }

    #[test]
    fn integer_range_violations_are_reported() {
        let node = Node::new("respond", origin()).with_prop("status", Value::int(70_000, origin()));
        let mut diagnostics = Diagnostics::new();
        let mut reader = NodeReader::new(&node, &mut diagnostics);
        let status: Option<u16> = reader.opt_prop("status");
        reader.finish();
        assert!(status.is_none());
        assert!(diagnostics.iter().any(|d| d.message.contains("out of range")));
    }

    #[test]
    fn wrong_scalar_type_names_both_sides() {
        let value = Value::new(Scalar::Bool(true), origin());
        let mut diagnostics = Diagnostics::new();
        assert!(String::from_value(&value, &mut diagnostics).is_none());
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message == "expected a string, found boolean")
        );
    }
}
