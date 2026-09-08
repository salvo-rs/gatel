use gatel_cgi_http::{ExchangeRequest, Protocol, Running, Sink, directive};
use gatel_model::{Diagnostics, Node, Origin, Value};
use gatel_plugin_runtime::{Directive, DirectiveMeta};

#[derive(Debug)]
struct TestProtocol;
impl Protocol for TestProtocol {
    fn meta(&self) -> &'static DirectiveMeta {
        static META: DirectiveMeta = DirectiveMeta::terminal("test-cgi");
        &META
    }
    fn run(&self, _: ExchangeRequest, _: Sink) -> Running {
        panic!("configuration decoding must never run an exchange")
    }
}

fn config() -> Node {
    let origin = Origin::synthetic("config-test");
    Node::new("test-cgi", origin.clone())
        .with_arg(Value::string("127.0.0.1:9000", origin.clone()))
        .with_prop("root", Value::string("nonexistent-config-root", origin.clone()))
        .with_prop("script", Value::string("app.marker", origin))
}

#[test]
fn remote_root_syntax_is_rejected_before_prepare() {
    let protocol = directive(TestProtocol);
    for path in [
        "relative",
        "/srv/../app",
        "/srv/./app",
        "C:/app",
        "/bad\\app",
        "/bad\0app",
        "/bad\napp",
    ] {
        let node = config().with_prop("remote-root", Value::string(path, Origin::synthetic("remote")));
        let mut diagnostics = Diagnostics::new();
        assert!(protocol.decode(&node, &mut diagnostics).is_none(), "{path:?}");
        assert!(diagnostics.iter().any(|d| d.message.contains("remote-root")));
    }
    for path in ["/", "/srv/app", "/srv/app/"] {
        let node = config().with_prop("remote-root", Value::string(path, Origin::synthetic("remote")));
        assert!(protocol.decode(&node, &mut Diagnostics::new()).is_some());
    }
}

#[test]
fn server_name_controls_are_configuration_errors() {
    for name in ["", "bad\0host", "bad\r\nhost"] {
        let node = config().with_prop("server-name", Value::string(name, Origin::synthetic("name")));
        assert!(
            directive(TestProtocol)
                .decode(&node, &mut Diagnostics::new())
                .is_none()
        );
    }
}

#[test]
fn idle_timeout_is_bounded_and_changes_the_configuration_fingerprint() {
    let protocol = directive(TestProtocol);
    let decode = |seconds| {
        protocol.decode(
            &config().with_prop("idle-timeout", Value::int(seconds, Origin::synthetic("idle"))),
            &mut Diagnostics::new(),
        )
    };
    for seconds in [0, -1, 3601] {
        assert!(decode(seconds).is_none());
    }
    let fingerprint = |seconds| {
        let mut hash = gatel_model::Hasher::new();
        decode(seconds).unwrap().hash_into(&mut hash);
        hash.finish()
    };
    assert_ne!(fingerprint(1), fingerprint(15));
    assert!(decode(3600).is_some());
}
