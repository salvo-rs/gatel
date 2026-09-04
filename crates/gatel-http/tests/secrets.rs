//! Nothing that reads a configuration may render what a secret points at.
//!
//! Two different guarantees are checked here, and they are not equally strong.
//!
//! The first is structural: a compiled plan holds a *reference* — `file:x`,
//! `env:Y` — and never the material behind it. Material only exists after
//! `build`, inside a handler that nothing renders. So the tests over everything
//! a plan can print are a guard on that shape rather than on redaction: they
//! fail if someone ever puts material into a `DirectiveConfig`.
//!
//! The second is the one that can actually go wrong, and it does bite. A
//! directive that reads a secret during prepare and finds it unusable has the
//! material in hand at exactly the moment it is writing an error message.
//! Quoting it there would put a password hash into a log, and this fails if it
//! ever does — verified by breaking it on purpose.

mod support;

use std::sync::Arc;

use gatel_model::{Snapshot, SourceId};
use gatel_plan::Registry;
use support::*;

/// Distinctive enough that finding it anywhere is unambiguous.
const SECRET: &str = "SENTINEL-a7f3c9e1-never-render-this";

fn registry() -> Registry {
    Registry::builder()
        .bundle(gatel_bundle_std::bundle())
        .build()
        .unwrap()
}

/// Everything a person or a log could see about a compiled configuration.
fn everything_rendered(plan: &gatel_plan::Plan) -> String {
    let mut out = String::new();
    for listener in &plan.listeners {
        out.push_str(&format!("{:?}\n", listener.listener));
        for site in &listener.sites {
            out.push_str(&format!("{:?}\n", site.tls));
            for route in &site.routes {
                out.push_str(&format!("{route:?}\n"));
                for directive in &route.chain {
                    out.push_str(&directive.config.describe());
                    out.push('\n');
                    out.push_str(&format!("{directive:?}\n"));
                }
                if let gatel_plan::Terminal::Directive(instance) = &route.terminal {
                    out.push_str(&instance.config.describe());
                    out.push('\n');
                }
            }
        }
    }
    for upstream in &plan.spec.upstreams {
        out.push_str(&format!("{upstream:?}\n"));
    }
    out.push_str(&format!("{:?}\n", plan.spec.global));
    out
}

fn compile(gateway: &Gateway, kdl: &str) -> Result<Arc<gatel_plan::Plan>, gatel_model::Diagnostics> {
    let path = gateway.directory.join("gatel.kdl");
    std::fs::write(&path, kdl).unwrap();
    let registry = registry();
    let source = gatel_kdl::snapshot(&path, SourceId::file("main"), None)?;
    let spec = gatel_plan::normalize(&Snapshot::new().with(source), &registry)?;
    Ok(Arc::new(gatel_plan::compile(Arc::new(spec), &registry)))
}

#[tokio::test]
async fn a_compiled_plan_carries_the_reference_and_never_the_material() {
    let gateway = Gateway::new("secrets-render");
    let port = free_port();
    // A real bcrypt hash, and a file whose contents are the sentinel.
    let hash = bcrypt::hash("hunter2", 4).unwrap();
    std::fs::write(gateway.directory.join("ops.hash"), &hash).unwrap();
    std::fs::write(gateway.directory.join("token.txt"), SECRET).unwrap();

    let plan = compile(
        &gateway,
        &format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" id="all" {{
    route "/*" id="any" {{
        basic-auth realm="ops" {{
            user "ops" hash="file:ops.hash"
        }}
        respond "ok\n"
    }}
}}
"#
        ),
    )
    .expect("the configuration is valid");

    let rendered = everything_rendered(&plan);
    assert!(
        !rendered.contains(&hash),
        "a password hash reached a rendering:\n{rendered}"
    );
    // The reference itself is not a secret and is worth showing.
    assert!(
        rendered.contains("basic-auth"),
        "the directive still describes itself:\n{rendered}"
    );
}

#[tokio::test]
async fn a_secret_that_is_wrong_is_reported_without_quoting_it() {
    let gateway = Gateway::new("secrets-diagnostic");
    let port = free_port();
    // Not a bcrypt hash at all, so building it has to fail.
    std::fs::write(gateway.directory.join("ops.hash"), SECRET).unwrap();

    let outcome = gateway
        .apply(&format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" id="all" {{
    route "/*" id="any" {{
        basic-auth realm="ops" {{
            user "ops" hash="file:ops.hash"
        }}
        respond "ok\n"
    }}
}}
"#
        ))
        .await;

    let reported = format!("{outcome:?}");
    assert!(
        reported.contains("PrepareFailed") || reported.contains("Rejected"),
        "the configuration should have been refused: {reported}"
    );
    assert!(
        !reported.contains(SECRET),
        "the material was quoted back in the error:\n{reported}"
    );
    // It still has to say which user and which route, or it is not actionable.
    assert!(reported.contains("ops"), "{reported}");
}

#[tokio::test]
async fn an_environment_secret_is_a_reference_too() {
    let gateway = Gateway::new("secrets-env");
    let port = free_port();
    // SAFETY: single-threaded setup before anything reads the environment.
    unsafe { std::env::set_var("GATEL_TEST_SENTINEL", SECRET) };

    let plan = compile(
        &gateway,
        &format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
site "*" id="all" {{
    route "/*" id="any" {{
        basic-auth realm="ops" {{
            user "ops" hash="env:GATEL_TEST_SENTINEL"
        }}
        respond "ok\n"
    }}
}}
"#
        ),
    )
    .expect("the configuration is valid");

    let rendered = everything_rendered(&plan);
    assert!(!rendered.contains(SECRET), "{rendered}");
    // Naming the variable is how an operator finds what to fix.
    assert!(rendered.contains("basic-auth"), "{rendered}");
    unsafe { std::env::remove_var("GATEL_TEST_SENTINEL") };
}

#[tokio::test]
async fn an_upstream_authority_path_is_shown_but_its_contents_are_not() {
    let gateway = Gateway::new("secrets-upstream");
    let port = free_port();
    let generated = rcgen::generate_simple_self_signed(vec!["api.internal".to_string()]).unwrap();
    std::fs::write(gateway.directory.join("ca.pem"), generated.cert.pem()).unwrap();

    let plan = compile(
        &gateway,
        &format!(
            r#"
global {{ http "127.0.0.1:{port}" }}
upstream "api" {{
    endpoint "10.0.0.1:443"
    tls server-name="api.internal" ca="file:ca.pem"
}}
site "*" id="all" {{ route "/*" id="any" {{ proxy upstream="api" }} }}
"#
        ),
    )
    .expect("the configuration is valid");

    let rendered = everything_rendered(&plan);
    assert!(
        !rendered.contains("BEGIN CERTIFICATE"),
        "certificate material reached a rendering:\n{rendered}"
    );
    assert!(
        rendered.contains("api.internal"),
        "what it reaches is still worth saying:\n{rendered}"
    );
}
