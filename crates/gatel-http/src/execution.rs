//! Stable per-route behavior namespace, excluding unrelated route edits.
use gatel_model::Hasher;
use gatel_plan::{ExecutionIdentity, Plan, RoutePlan, Terminal};

pub(crate) fn identity(route: &RoutePlan, plan: &Plan) -> ExecutionIdentity {
    let mut hash = Hasher::new();
    hash.update_str("execution-identity-v1");
    route.id.hash_into(&mut hash);
    hash.update_str(&route.path.describe());
    hash.update_str(&route.methods.describe());
    hash.update_u64(route.matchers.len() as u64);
    for matcher in &route.matchers {
        matcher.hash_into(&mut hash);
    }
    plan.spec.global.hash_into(&mut hash);
    let mut dependencies = std::collections::BTreeSet::new();
    hash.update_u64(route.chain.len() as u64);
    for directive in &route.chain {
        directive.hash_into(&mut hash);
        dependencies.extend(directive.config.upstreams());
    }
    route.terminal.hash_into(&mut hash);
    match &route.terminal {
        Terminal::Proxy { upstream } => {
            dependencies.insert(upstream.clone());
        }
        Terminal::Directive(instance) => {
            dependencies.extend(instance.config.upstreams());
        }
    }
    hash.update_u64(dependencies.len() as u64);
    for id in dependencies {
        id.hash_into(&mut hash);
        if let Some(upstream) = plan.upstream(&id) {
            upstream.hash_into(&mut hash);
        }
    }
    ExecutionIdentity(hash.finish())
}
