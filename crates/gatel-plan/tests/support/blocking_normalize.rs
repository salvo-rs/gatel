use super::*;

#[test]
fn blocking_budget_is_typed_bounded_and_changes_the_fingerprint() {
    let config = |budget: &str| {
        format!("global {{ http \":8080\"; {budget} }}\nsite \"*\" {{ route \"/\" {{ respond \"hi\" }} }}\n")
    };
    let first = spec(&config("blocking jobs=2 memory=\"16KiB\";"));
    let limits = first.global.blocking.unwrap();
    assert_eq!(limits.jobs, 2);
    assert_eq!(limits.memory_bytes, 16384);
    let mut a = gatel_model::Hasher::new();
    first.global.hash_into(&mut a);
    let mut b = gatel_model::Hasher::new();
    spec(&config("blocking jobs=3 memory=\"16KiB\";"))
        .global
        .hash_into(&mut b);
    assert_ne!(a.finish(), b.finish());
    for invalid in [
        "blocking jobs=0;",
        "blocking jobs=1025;",
        "blocking jobs=-1;",
        "blocking memory=\"0B\";",
        "blocking jobs=\"two\";",
        "blocking unknown=1;",
        "blocking; blocking;",
    ] {
        assert!(
            gatel_plan::normalize(&snapshot(&config(invalid)), &registry()).is_err(),
            "{invalid}"
        );
    }
    assert_eq!(
        spec(&config("blocking;")).global.blocking,
        Some(Default::default())
    );
    assert!(spec(&config("")).global.blocking.is_none());
}
