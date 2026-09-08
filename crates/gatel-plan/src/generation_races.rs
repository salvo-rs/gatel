//! Regression cases for late observations and failed/overtaken candidates.
use gatel_model::{Hasher, SourceId, SourceRev, SourceSnapshot};

use super::tests::*;
use super::*;

struct BudgetRuntime(crate::blocking::PreparedChange);
impl crate::publication::PublishRuntime for BudgetRuntime {
    type Guard<'a> = crate::blocking::Publication<'a>;
    fn begin_publication(&self) -> Self::Guard<'_> {
        self.0.begin_publication()
    }
}

#[test]
fn only_the_winning_generation_publishes_its_prepared_budget() {
    let owner = crate::blocking::Budget::new(Default::default()).unwrap();
    let executor = owner.executor();
    let store = GenerationStore::<BudgetRuntime>::new();
    let registry = registry();
    let prepared = |jobs| {
        BudgetRuntime(
            owner
                .prepare(
                    &executor,
                    crate::blocking::Limits {
                        jobs,
                        memory_bytes: 100,
                    },
                )
                .unwrap(),
        )
    };
    store.attempt(&snapshot(1, "a", 8080), &registry, |_| Ok(prepared(2)));
    assert_eq!(executor.snapshot().limits.jobs, 2);
    let failed = store.attempt(&snapshot(2, "b", 8080), &registry, |_| {
        let _inert = prepared(3);
        Err("fixture failed after staging limits".into())
    });
    assert!(matches!(failed, Outcome::PrepareFailed(_)));
    assert_eq!(executor.snapshot().limits.jobs, 2);
    let stale = store.attempt(&snapshot(3, "c", 8080), &registry, |_| {
        let outer = prepared(4);
        let winner = store.attempt(&snapshot(4, "d", 8080), &registry, |_| Ok(prepared(1)));
        assert!(matches!(winner, Outcome::Reloaded { .. }));
        Ok(outer)
    });
    assert!(matches!(stale, Outcome::Stale(_)));
    assert_eq!(executor.snapshot().limits.jobs, 1);
    let noop = store.attempt(&snapshot(5, "d", 8080), &registry, |_| {
        panic!("no-op must not stage")
    });
    assert!(matches!(noop, Outcome::NoOp { .. }));
    assert_eq!(executor.snapshot().limits.jobs, 1);
}

struct DecodeGate {
    meta: crate::DirectiveMeta,
    entered: Arc<std::sync::Barrier>,
    released: Arc<std::sync::Barrier>,
}
struct Text(String);
impl crate::DirectiveConfig for Text {
    fn hash_into(&self, hash: &mut Hasher) {
        hash.update_str(&self.0);
    }
    fn describe(&self) -> String {
        self.0.clone()
    }
}
impl crate::Directive for DecodeGate {
    fn meta(&self) -> &crate::DirectiveMeta {
        &self.meta
    }
    fn decode(
        &self,
        node: &gatel_model::Node,
        diagnostics: &mut Diagnostics,
    ) -> Option<Arc<dyn crate::DirectiveConfig>> {
        let text = node.args.first()?.as_str()?;
        if text == "slow-invalid" {
            self.entered.wait();
            self.released.wait();
            diagnostics.error(
                gatel_model::Code::InvalidValue,
                node.origin.clone(),
                "late decode failure",
            );
            None
        } else {
            Some(Arc::new(Text(text.into())))
        }
    }
}

#[test]
fn a_normalization_error_overtaken_on_another_thread_is_stale() {
    let entered = Arc::new(std::sync::Barrier::new(2));
    let released = Arc::new(std::sync::Barrier::new(2));
    let registry = Registry::builder()
        .bundle(crate::Bundle::new("gate", "1").with(DecodeGate {
            meta: crate::DirectiveMeta::terminal("respond"),
            entered: entered.clone(),
            released: released.clone(),
        }))
        .build()
        .unwrap();
    let store = store();
    store.attempt(&snapshot(1, "old", 8080), &registry, prepare_ok);
    std::thread::scope(|scope| {
        let slow = scope.spawn(|| store.attempt(&snapshot(2, "slow-invalid", 8080), &registry, prepare_ok));
        entered.wait();
        let newer = store.attempt(&snapshot(3, "new", 8080), &registry, prepare_ok);
        released.wait();
        assert!(matches!(newer, Outcome::Reloaded { .. }));
        assert!(matches!(slow.join().unwrap(), Outcome::Stale(_)));
    });
    assert_eq!(store.status().generation.number(), 2);
    assert!(!store.status().is_degraded());
}

#[test]
fn an_already_obsolete_snapshot_cannot_prepare_or_replace_current() {
    let store = store();
    let registry = registry();
    store.attempt(&snapshot(3, "new", 8080), &registry, prepare_ok);
    let before = store.current().unwrap();
    let outcome = store.attempt(&snapshot(2, "old", 8080), &registry, |_| {
        panic!("an obsolete observation must not reach preparation")
    });
    assert!(matches!(outcome, Outcome::Stale(_)), "{outcome:?}");
    assert!(Arc::ptr_eq(&before, &store.current().unwrap()));
    assert_eq!(
        store.status().revs.get(&SourceId::file("main")),
        Some(SourceRev(3))
    );
}

#[test]
fn an_old_noop_does_not_clear_a_newer_rejection() {
    let store = store();
    let registry = registry();
    store.attempt(&snapshot(1, "good", 8080), &registry, prepare_ok);
    store.attempt(&invalid_snapshot(2), &registry, prepare_ok);
    let before = store.status().last_failure.unwrap();
    let outcome = store.attempt(&snapshot(1, "good", 8080), &registry, prepare_ok);
    assert!(matches!(outcome, Outcome::Stale(_)), "{outcome:?}");
    let after = store.status().last_failure.unwrap();
    assert_eq!(before.at, after.at);
    assert_eq!(before.summary, after.summary);
}

#[test]
fn a_late_prepare_error_cannot_mark_a_newer_success_unhealthy() {
    let store = store();
    let registry = registry();
    store.attempt(&snapshot(1, "a", 8080), &registry, prepare_ok);
    let outcome = store.attempt(&snapshot(2, "b", 8080), &registry, |_| {
        assert!(matches!(
            store.attempt(&snapshot(3, "c", 8080), &registry, prepare_ok),
            Outcome::Reloaded { .. }
        ));
        Err("late failure".into())
    });
    assert!(matches!(outcome, Outcome::Stale(_)), "{outcome:?}");
    assert_eq!(store.status().generation.number(), 2);
    assert!(!store.status().is_degraded());
}

#[test]
fn a_late_prepare_error_cannot_replace_a_newer_failure() {
    let store = store();
    let registry = registry();
    store.attempt(&snapshot(1, "a", 8080), &registry, prepare_ok);
    let outcome = store.attempt(&snapshot(2, "b", 8080), &registry, |_| {
        let newer = store.attempt(&snapshot(3, "c", 8080), &registry, |_| Err("new failure".into()));
        assert!(matches!(newer, Outcome::PrepareFailed(_)));
        Err("old failure".into())
    });
    assert!(matches!(outcome, Outcome::Stale(_)), "{outcome:?}");
    assert_eq!(store.status().last_failure.unwrap().summary, "new failure");
}

#[test]
fn missing_known_sources_are_stale_and_empty_revisions_are_explicit_removal() {
    let store = store();
    let registry = registry();
    let admin = |rev| {
        let mut content = Hasher::new();
        content.update_u64(rev);
        let nodes = if rev == 1 {
            let fixture = snapshot(1, "admin contribution", 8080);
            let mut site = fixture.get(&SourceId::file("main")).unwrap().nodes[1].clone();
            site.args[0] = gatel_model::Value::string("admin.example", site.origin.clone());
            vec![site]
        } else {
            vec![]
        };
        SourceSnapshot::new(SourceId::admin(), SourceRev(rev), content.finish(), nodes)
    };
    store.attempt(&snapshot(1, "a", 8080).with(admin(1)), &registry, prepare_ok);
    let outcome = store.attempt(&snapshot(2, "a", 8080), &registry, prepare_ok);
    assert!(matches!(outcome, Outcome::Stale(_)), "{outcome:?}");
    let outcome = store.attempt(&snapshot(2, "a", 8080).with(admin(2)), &registry, prepare_ok);
    assert!(matches!(outcome, Outcome::Reloaded { .. }), "{outcome:?}");
    assert_eq!(store.status().revs.get(&SourceId::admin()), Some(SourceRev(2)));
}

#[test]
fn two_candidates_for_the_same_revision_only_publish_once() {
    let store = store();
    let registry = registry();
    store.attempt(&snapshot(1, "a", 8080), &registry, prepare_ok);
    let candidate = snapshot(2, "b", 8080);
    let outer = store.attempt(&candidate, &registry, |preparation| {
        let inner = store.attempt(&candidate, &registry, prepare_ok);
        assert!(matches!(inner, Outcome::Reloaded { .. }));
        prepare_ok(preparation)
    });
    assert!(matches!(outer, Outcome::Stale(_)));
    assert_eq!(store.status().generation.number(), 2);
}

#[test]
fn an_error_after_a_same_revision_commit_is_also_stale() {
    let store = store();
    let registry = registry();
    store.attempt(&snapshot(1, "a", 8080), &registry, prepare_ok);
    let candidate = snapshot(2, "b", 8080);
    let outcome = store.attempt(&candidate, &registry, |_| {
        assert!(matches!(
            store.attempt(&candidate, &registry, prepare_ok),
            Outcome::Reloaded { .. }
        ));
        Err("late preparation failure at the same source revision".into())
    });
    assert!(matches!(outcome, Outcome::Stale(_)));
    assert!(!store.status().is_degraded());
}
