//! The derive macro is the single decoding path for every frontend, so its
//! behaviour is pinned here rather than only through the directives that use it.

use std::time::Duration;

use gatel_macros::FromNode;
use gatel_model::{ByteSize, Code, Diagnostics, Node, Origin, Value};

fn origin() -> Origin {
    Origin::synthetic("test")
}

fn node(name: &str) -> Node {
    Node::new(name, origin())
}

fn decode<T: gatel_model::FromNode>(node: &Node) -> (Option<T>, Diagnostics) {
    let mut diagnostics = Diagnostics::new();
    let value = T::from_node(node, &mut diagnostics);
    (value, diagnostics)
}

#[derive(Debug, PartialEq, FromNode)]
struct Respond {
    #[gatel(arg, default)]
    body: String,
    #[gatel(prop, default = 200)]
    status: u16,
    #[gatel(prop)]
    content_type: Option<String>,
}

#[test]
fn defaults_apply_when_nothing_is_written() {
    let (value, diagnostics) = decode::<Respond>(&node("respond"));
    assert!(!diagnostics.has_errors(), "{diagnostics}");
    assert_eq!(
        value.unwrap(),
        Respond {
            body: String::new(),
            status: 200,
            content_type: None
        }
    );
}

#[test]
fn positional_and_named_values_are_read() {
    let raw = node("respond")
        .with_arg(Value::string("hi", origin()))
        .with_prop("status", Value::int(404, origin()))
        .with_prop("content-type", Value::string("text/plain", origin()));
    let (value, diagnostics) = decode::<Respond>(&raw);
    assert!(!diagnostics.has_errors(), "{diagnostics}");
    assert_eq!(
        value.unwrap(),
        Respond {
            body: "hi".into(),
            status: 404,
            content_type: Some("text/plain".into()),
        }
    );
}

#[test]
fn field_names_become_kebab_case() {
    let raw = node("respond").with_prop("content_type", Value::string("x", origin()));
    let (value, diagnostics) = decode::<Respond>(&raw);
    assert!(value.is_none());
    let diagnostic = diagnostics.iter().next().unwrap();
    assert_eq!(diagnostic.code, Code::UnknownProperty);
    assert_eq!(diagnostic.help.as_deref(), Some("did you mean `content-type`?"));
}

#[derive(Debug, PartialEq, FromNode)]
struct Timeouts {
    #[gatel(prop)]
    connect: Option<Duration>,
    #[gatel(prop, default = ByteSize::mib(16))]
    max_body: ByteSize,
}

#[derive(Debug, PartialEq, FromNode)]
struct Upstream {
    #[gatel(arg)]
    name: String,
    #[gatel(children)]
    endpoint: Vec<Endpoint>,
    #[gatel(child)]
    timeouts: Option<Timeouts>,
}

#[derive(Debug, PartialEq, FromNode)]
struct Endpoint {
    #[gatel(arg)]
    address: String,
    #[gatel(prop, default = 1)]
    weight: u32,
}

#[test]
fn nested_children_decode_recursively() {
    let raw = node("upstream")
        .with_arg(Value::string("api", origin()))
        .with_child(
            node("endpoint")
                .with_arg(Value::string("10.0.0.1:80", origin()))
                .with_prop("weight", Value::int(3, origin())),
        )
        .with_child(node("endpoint").with_arg(Value::string("10.0.0.2:80", origin())))
        .with_child(node("timeouts").with_prop("connect", Value::string("5s", origin())));
    let (value, diagnostics) = decode::<Upstream>(&raw);
    assert!(!diagnostics.has_errors(), "{diagnostics}");
    let value = value.unwrap();
    assert_eq!(value.name, "api");
    assert_eq!(value.endpoint.len(), 2);
    assert_eq!(value.endpoint[0].weight, 3);
    assert_eq!(value.endpoint[1].weight, 1);
    let timeouts = value.timeouts.unwrap();
    assert_eq!(timeouts.connect, Some(Duration::from_secs(5)));
    assert_eq!(timeouts.max_body, ByteSize::mib(16));
}

#[test]
fn a_missing_required_argument_is_reported_once() {
    let (value, diagnostics) = decode::<Upstream>(&node("upstream"));
    assert!(value.is_none());
    assert_eq!(diagnostics.error_count(), 1);
    assert_eq!(diagnostics.iter().next().unwrap().code, Code::ArgumentCount);
}

#[test]
fn an_error_deep_in_a_child_fails_the_parent() {
    let raw = node("upstream")
        .with_arg(Value::string("api", origin()))
        .with_child(node("endpoint").with_prop("weight", Value::int(-1, origin())));
    let (value, diagnostics) = decode::<Upstream>(&raw);
    assert!(value.is_none());
    assert!(diagnostics.error_count() >= 1);
}

#[test]
fn a_repeated_single_child_is_rejected_with_both_locations() {
    let raw = node("upstream")
        .with_arg(Value::string("api", origin()))
        .with_child(node("timeouts"))
        .with_child(node("timeouts"));
    let (_, diagnostics) = decode::<Upstream>(&raw);
    let diagnostic = diagnostics.iter().find(|d| d.code == Code::Duplicate).unwrap();
    assert!(diagnostic.related.is_some());
}

#[derive(Debug, PartialEq, FromNode)]
struct Method {
    #[gatel(args)]
    names: Vec<String>,
}

#[test]
fn rest_arguments_collect_everything() {
    let raw = node("method")
        .with_arg(Value::string("GET", origin()))
        .with_arg(Value::string("POST", origin()))
        .with_arg(Value::string("HEAD", origin()));
    let (value, diagnostics) = decode::<Method>(&raw);
    assert!(!diagnostics.has_errors(), "{diagnostics}");
    assert_eq!(value.unwrap().names, ["GET", "POST", "HEAD"]);
}

#[derive(Debug, PartialEq, FromNode)]
struct Site {
    #[gatel(arg)]
    host: String,
    #[gatel(rest_children)]
    directives: Vec<Node>,
}

#[test]
fn rest_children_are_captured_without_being_decoded() {
    let raw = node("site")
        .with_arg(Value::string("example.com", origin()))
        .with_child(node("headers"))
        .with_child(node("anything-at-all"));
    let (value, diagnostics) = decode::<Site>(&raw);
    assert!(!diagnostics.has_errors(), "{diagnostics}");
    let value = value.unwrap();
    assert_eq!(value.host, "example.com");
    assert_eq!(value.directives.len(), 2);
    assert_eq!(value.directives[1].name, "anything-at-all");
}

#[derive(Debug, PartialEq, FromNode)]
struct Renamed {
    #[gatel(prop, rename = "max")]
    maximum: u32,
}

#[test]
fn rename_overrides_the_derived_key() {
    let raw = node("rate-limit").with_prop("max", Value::int(10, origin()));
    let (value, diagnostics) = decode::<Renamed>(&raw);
    assert!(!diagnostics.has_errors(), "{diagnostics}");
    assert_eq!(value.unwrap().maximum, 10);
}
