use super::tests::*;

#[test]
fn late_success_notifications_do_not_regress_generation_but_still_count_attempts() {
    let store = store();
    let registry = registry();
    let telemetry = gatel_observe::Telemetry::new();
    let first = store.attempt(&snapshot(1, "first", 8080), &registry, prepare_ok);
    let noop = store.attempt(&snapshot(2, "first", 8080), &registry, prepare_ok);
    let second = store.attempt(&snapshot(3, "second", 8080), &registry, prepare_ok);
    second.record(&telemetry);
    first.record(&telemetry);
    noop.record(&telemetry);
    assert_eq!(telemetry.metrics.process.generation.get(), 2);
    for kind in [
        gatel_observe::Reload::Started,
        gatel_observe::Reload::Reloaded,
        gatel_observe::Reload::NoOp,
    ] {
        assert_eq!(telemetry.metrics.process.reloads(kind), 1);
    }
}
