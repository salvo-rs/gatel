//! `gatel explain` and `gatel diff`.
//!
//! Both print the compiled plan rather than the file, because the question an
//! operator actually has is "what will this do?", and the answer involves
//! ordering and defaults that the file does not show.

use gatel_plan::{BindingChange, Plan, Terminal};

/// Render the plan the way the request path sees it.
pub fn explain(plan: &Plan) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "spec  {}\nplan  {}\n",
        plan.spec_hash().short(),
        plan.plan_hash().short()
    ));

    for listener in &plan.listeners {
        out.push_str(&format!(
            "\nlistener {} {} ({})\n",
            listener.listener.name,
            listener.listener.bind,
            listener.listener.protocol.as_str()
        ));
        if listener.sites.is_empty() {
            out.push_str("  (no sites)\n");
        }
        for site in &listener.sites {
            out.push_str(&format!("  site {}{}\n", site.host.describe(), tls_note(site)));
            for route in &site.routes {
                out.push_str(&format!(
                    "    route {} {} [{}]\n",
                    route.methods.describe(),
                    route.path.describe(),
                    route.id.name()
                ));
                for matcher in &route.matchers {
                    out.push_str(&format!("      when {}\n", matcher.describe()));
                }
                // The chain is printed in execution order, which is the whole
                // point: it is not the order the file lists them in.
                for directive in &route.chain {
                    out.push_str(&format!(
                        "      {:<8} {:<12} {}\n",
                        directive
                            .meta
                            .role
                            .phase()
                            .map(|phase| phase.as_str())
                            .unwrap_or("terminal"),
                        directive.meta.name,
                        directive.config.describe()
                    ));
                }
                out.push_str(&format!("      terminal     {}\n", route.terminal.describe()));
            }
        }
    }

    if !plan.upstreams.is_empty() {
        out.push_str("\nupstreams\n");
        for upstream in &plan.upstreams {
            out.push_str(&format!(
                "  {} {} endpoints, balance {}, pool {}/endpoint\n",
                upstream.id.name(),
                upstream.endpoints.len(),
                upstream.balance.describe(),
                upstream.pool.max_per_endpoint
            ));
            for endpoint in &upstream.endpoints {
                // The target as written, so a name reads as a name. What it
                // resolves to is a runtime fact and belongs in the metrics,
                // not in an explanation of the configuration.
                out.push_str(&format!(
                    "    {} weight {} priority {}\n",
                    endpoint.target.describe(),
                    endpoint.weight,
                    endpoint.priority
                ));
            }
            if upstream.has_names() {
                out.push_str(&format!(
                    "    names looked up every {:?} ({})\n",
                    upstream.resolve.interval,
                    upstream.resolve.family.as_str()
                ));
            }
            // Worth printing because the answer to "why did my backend see
            // this twice" is here and nowhere else in the file.
            match &upstream.retry {
                Some(retry) => out.push_str(&format!(
                    "    retry {} up to {} attempts, {:?} each, at most {}% of live traffic\n",
                    retry.mode.as_str(),
                    retry.attempts,
                    retry.per_try,
                    retry.budget_percent
                )),
                None => out.push_str("    no retry\n"),
            }
        }
    }

    out.push_str("\nstate\n");
    for binding in &plan.bindings {
        out.push_str(&format!("  {} {}\n", binding.owner, binding.kind.as_str()));
    }
    out
}

/// Render the difference between two plans, including what it would do to
/// running state.
pub fn diff(before: &Plan, after: &Plan) -> String {
    let mut out = String::new();
    if before.plan_hash() == after.plan_hash() {
        return "no change\n".to_string();
    }
    if before.spec_hash() == after.spec_hash() {
        out.push_str("the configuration is the same; the difference is in the build\n");
    }
    diff_resources(&mut out, &before.spec, &after.spec);

    let listeners = |plan: &Plan| -> Vec<String> {
        plan.listeners
            .iter()
            .map(|listener| format!("{} {}", listener.listener.name, listener.listener.bind))
            .collect()
    };
    diff_lines(&mut out, "listeners", &listeners(before), &listeners(after));

    let routes = |plan: &Plan| -> Vec<String> {
        plan.listeners
            .iter()
            .flat_map(|listener| listener.sites.iter())
            .flat_map(|site| {
                site.routes.iter().map(move |route| {
                    format!(
                        "{} {} {} -> {}",
                        site.host.describe(),
                        route.methods.describe(),
                        route.path.describe(),
                        terminal_summary(&route.terminal)
                    )
                })
            })
            .collect()
    };
    diff_lines(&mut out, "routes", &routes(before), &routes(after));

    let changes = gatel_plan::diff_bindings(&before.bindings, &after.bindings);
    let mut state = Vec::new();
    for change in &changes {
        match change {
            BindingChange::Reuse(_) => {}
            BindingChange::Create(binding) => {
                state.push(format!("  + {} {}", binding.owner, binding.kind.as_str()));
            }
            BindingChange::Replace { binding, reason } => {
                state.push(format!(
                    "  ~ {} {} (restarted: {reason})",
                    binding.owner,
                    binding.kind.as_str()
                ));
            }
            BindingChange::Retire(binding) => {
                state.push(format!("  - {} {}", binding.owner, binding.kind.as_str()));
            }
        }
    }
    if state.is_empty() {
        out.push_str("state\n  all reused\n");
    } else {
        out.push_str("state\n");
        for line in state {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

/// Compare normalized resources without printing plugin configuration values
/// or hashes derived from those values (which may contain sensitive data).
fn diff_resources(out: &mut String, before: &gatel_plan::Spec, after: &gatel_plan::Spec) {
    use std::collections::BTreeMap;

    use gatel_model::{Fingerprint, Hasher, ResourceId, SourceId};
    fn resources(spec: &gatel_plan::Spec) -> BTreeMap<ResourceId, (SourceId, Fingerprint)> {
        let mut values = BTreeMap::new();
        macro_rules! collect {
            ($items:expr) => {
                for item in $items {
                    let mut hash = Hasher::new();
                    item.hash_into(&mut hash);
                    values.insert(item.id.clone(), (item.owner.clone(), hash.finish()));
                }
            };
        }
        collect!(&spec.listeners);
        collect!(&spec.sites);
        collect!(&spec.upstreams);
        values
    }
    let mut changes = String::new();
    let mut old_global = Hasher::new();
    let mut new_global = Hasher::new();
    before.global.hash_into(&mut old_global);
    after.global.hash_into(&mut new_global);
    if old_global.finish() != new_global.finish() {
        changes.push_str("  ~ global configuration changed\n");
    }
    let old = resources(before);
    let new = resources(after);
    for (id, (owner, hash)) in &old {
        match new.get(id) {
            None => changes.push_str(&format!("  - {id} (owner {owner})\n")),
            Some((next_owner, next_hash)) => {
                if owner != next_owner {
                    changes.push_str(&format!("  ~ {id} owner {owner} -> {next_owner}\n"));
                }
                if hash != next_hash {
                    changes.push_str(&format!("  ~ {id} configuration changed\n"));
                }
            }
        }
    }
    for (id, (owner, _)) in &new {
        if !old.contains_key(id) {
            changes.push_str(&format!("  + {id} (owner {owner})\n"));
        }
    }
    if !changes.is_empty() {
        out.push_str("resources\n");
        out.push_str(&changes);
    }
}

fn terminal_summary(terminal: &Terminal) -> String {
    match terminal {
        Terminal::Proxy { upstream } => format!("proxy {}", upstream.name()),
        Terminal::Directive(instance) => instance.meta.name.to_string(),
    }
}

fn diff_lines(out: &mut String, label: &str, before: &[String], after: &[String]) {
    let removed: Vec<&String> = before.iter().filter(|line| !after.contains(line)).collect();
    let added: Vec<&String> = after.iter().filter(|line| !before.contains(line)).collect();
    if removed.is_empty() && added.is_empty() {
        return;
    }
    out.push_str(label);
    out.push('\n');
    for line in removed {
        out.push_str(&format!("  - {line}\n"));
    }
    for line in added {
        out.push_str(&format!("  + {line}\n"));
    }
}

/// Where a site's certificate comes from, when that is worth saying.
///
/// The automatic case earns a note because nothing in the file mentions it: a
/// name that can only ever be local was given a development certificate so it
/// could be served at all, and the reader should not have to work that out.
fn tls_note(site: &gatel_plan::SitePlan) -> String {
    match &site.tls {
        None => String::new(),
        Some(tls) => {
            let source = match &tls.source {
                gatel_plan::CertificateSource::Manual { .. } => "tls from files",
                gatel_plan::CertificateSource::Internal if tls.automatic => {
                    return "  (tls internal, automatic — run `gatel trust`)".to_string();
                }
                gatel_plan::CertificateSource::Internal => "tls internal",
                gatel_plan::CertificateSource::Acme => "tls acme",
            };
            format!("  ({source})")
        }
    }
}
