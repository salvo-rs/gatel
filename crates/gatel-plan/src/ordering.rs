//! Deterministic plugin composition. Constraints override user order, while
//! phase boundaries within a scope remain mandatory. Cross-scope requirements
//! can wrap a site's consumer in a route's guard.
use gatel_model::{Code, Diagnostic, Diagnostics};

use crate::DirectiveInstance;

/// Validate the fixed terminal boundary, then order the middleware. Terminals
/// never participate in scope sorting and cannot be moved ahead of middleware.
pub fn order_route(
    chain: Vec<DirectiveInstance>,
    terminal: &crate::Terminal,
) -> Result<Vec<DirectiveInstance>, Diagnostics> {
    let mut diagnostics = Diagnostics::new();
    let terminal_meta = match terminal {
        crate::Terminal::Directive(instance) => Some(&instance.meta),
        crate::Terminal::Proxy { .. } => None,
    };
    for item in &chain {
        let conflict = item.meta.conflicts.contains(&terminal.name())
            || terminal_meta.is_some_and(|meta| meta.conflicts.contains(&item.meta.name));
        let reverse = terminal_meta.is_some_and(|meta| {
            meta.before.contains(&item.meta.name)
                || meta
                    .effects
                    .iter()
                    .any(|left| item.meta.effects.iter().any(|right| left.must_precede(*right)))
        });
        if conflict || reverse {
            let message = if conflict {
                format!(
                    "`{}` conflicts with terminal `{}`",
                    item.meta.name,
                    terminal.name()
                )
            } else {
                format!(
                    "terminal `{}` requires execution before `{}`, but terminals must execute last",
                    terminal.name(),
                    item.meta.name,
                )
            };
            let mut diagnostic = Diagnostic::error(Code::InvalidValue, item.origin.clone(), message);
            if let crate::Terminal::Directive(instance) = terminal {
                diagnostic = diagnostic.with_related("terminal directive", instance.origin.clone());
            }
            diagnostics.push(diagnostic);
        }
    }
    if diagnostics.has_errors() {
        return Err(diagnostics);
    }
    order_chain(chain)
}

/// Compose middleware only; terminal selection is handled separately by the plan.
pub fn order_chain(mut chain: Vec<DirectiveInstance>) -> Result<Vec<DirectiveInstance>, Diagnostics> {
    chain.sort_by_key(DirectiveInstance::sort_key);
    let mut diagnostics = Diagnostics::new();
    let n = chain.len();
    let mut edges = vec![vec![false; n]; n];
    let mut incoming = vec![0usize; n];
    for (i, a) in chain.iter().enumerate() {
        for (j, b) in chain.iter().enumerate() {
            if i == j {
                continue;
            }
            if a.meta.conflicts.contains(&b.meta.name) {
                diagnostics.push(
                    Diagnostic::error(
                        Code::InvalidValue,
                        a.origin.clone(),
                        format!(
                            "`{}` conflicts with `{}` in this execution chain",
                            a.meta.name, b.meta.name
                        ),
                    )
                    .with_related("conflicting directive", b.origin.clone()),
                );
            }
            let phase_before = a.scope == b.scope && a.meta.role.phase() < b.meta.role.phase();
            let effect_before = a
                .meta
                .effects
                .iter()
                .any(|left| b.meta.effects.iter().any(|right| left.must_precede(*right)));
            // A site cache cannot short-circuit a route's authentication or
            // quota. Guard semantics hold across scopes, not only within one.
            let guard_before_cache = a.meta.role.phase() == Some(crate::Phase::Guard)
                && b.meta.effects.contains(&crate::Effect::CacheLookup);
            edges[i][j] =
                a.meta.before.contains(&b.meta.name) || phase_before || effect_before || guard_before_cache;
            incoming[j] += usize::from(edges[i][j]);
        }
    }
    if diagnostics.has_errors() {
        return Err(diagnostics);
    }
    let mut selected = vec![false; n];
    let mut result = Vec::with_capacity(n);
    while result.len() < n {
        let next = (0..n).find(|&j| !selected[j] && incoming[j] == 0);
        let Some(next) = next else {
            let blocked = (0..n)
                .filter(|&i| !selected[i])
                .map(|i| chain[i].meta.name)
                .collect::<Vec<_>>();
            diagnostics.error(
                Code::InvalidValue,
                chain[(0..n).find(|&i| !selected[i]).unwrap()].origin.clone(),
                format!("plugin ordering cycle: {}", blocked.join(" -> ")),
            );
            return Err(diagnostics);
        };
        selected[next] = true;
        for (j, count) in incoming.iter_mut().enumerate() {
            *count -= usize::from(edges[next][j]);
        }
        result.push(chain[next].clone());
    }
    Ok(result)
}
