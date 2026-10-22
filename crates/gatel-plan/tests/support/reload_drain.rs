use super::*;

#[test]
fn reload_drain_is_opt_in_bounded_and_changes_the_plan() {
    let base = "global { http \":8080\" }\nsite \"*\" { route \"/\" { respond \"ok\" } }\n";
    let ordinary = spec(base);
    assert_eq!(ordinary.global.reload_drain, None);
    let changed = base.replace("http \":8080\"", "http \":8080\"; reload-drain \"30s\"");
    let draining = spec(&changed);
    assert_eq!(
        draining.global.reload_drain,
        Some(std::time::Duration::from_secs(30))
    );
    assert_ne!(
        compile(Arc::new(ordinary), &registry()).plan_hash(),
        compile(Arc::new(draining), &registry()).plan_hash()
    );
    assert!(has(
        &errors(&changed.replace("30s", "999999999s")),
        Code::InvalidValue
    ));
    assert!(has(
        &errors(&changed.replace(
            "reload-drain \"30s\"",
            "reload-drain \"30s\"; reload-drain \"1s\""
        )),
        Code::Duplicate
    ));
    assert!(has(
        &errors(&changed.replace("\"30s\"", "\"30s\" typo=1")),
        Code::UnknownProperty
    ));
    assert_eq!(
        spec(&changed.replace("30s", "0s")).global.reload_drain,
        Some(std::time::Duration::ZERO)
    );
}
