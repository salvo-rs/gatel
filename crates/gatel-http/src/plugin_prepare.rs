//! One prepared handler/task bundle for each configuration owner, independent
//! of how many routes and listeners execute that owner.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use gatel_model::{Fingerprint, Hasher, ResourceId};
use gatel_plan::{BackgroundTask, DirectiveInstance, Plan, SharedUpstreams, Terminal};
use salvo_core::Handler;

use crate::Resources;
use crate::plugin_tasks::PluginTasks;

pub(crate) struct PreparedPlugins {
    pub secrets: Arc<gatel_plan::SecretDependencies>,
    handlers: BTreeMap<ResourceId, (Fingerprint, Arc<dyn Handler>)>,
    tasks: Vec<BackgroundTask>,
}
impl PreparedPlugins {
    pub fn prepare(
        plan: &Plan,
        resources: &Resources<'_>,
        reachable: &SharedUpstreams,
        sinks: &gatel_observe::sink::StagedSinks,
    ) -> Result<Self, String> {
        let mut prepared = Self {
            secrets: Arc::default(),
            handlers: BTreeMap::new(),
            tasks: Vec::new(),
        };
        for listener in &plan.listeners {
            for site in &listener.sites {
                let directives = site.fallback.iter().chain(site.routes.iter().flat_map(|route| {
                    route.chain.iter().chain(match &route.terminal {
                        Terminal::Directive(instance) => Some(instance),
                        Terminal::Proxy { .. } => None,
                    })
                }));
                for directive in directives {
                    let mut hash = Hasher::new();
                    directive.hash_into(&mut hash);
                    let identity = hash.finish();
                    if let Some((existing, _)) = prepared.handlers.get(&directive.owner) {
                        if existing != &identity {
                            return Err(format!(
                                "conflicting plugin configurations share owner `{}` ({})",
                                directive.owner.name(),
                                directive.meta.name
                            ));
                        }
                        continue;
                    }
                    let scoped = gatel_plan::ScopedUpstreams::restrict(
                        reachable.clone(),
                        directive.config.upstreams(),
                    );
                    let cx = gatel_plan::BuildContext {
                        secrets: Arc::clone(&prepared.secrets),
                        blocking: resources.blocking.clone(),
                        root: resources.root,
                        state: crate::prepare::resolve_state(directive, resources.state),
                        telemetry: resources.telemetry,
                        log_sinks: Some(sinks),
                        upstreams: &scoped,
                    };
                    let bundle = directive.config.prepare(cx).map_err(|error| {
                        format!(
                            "`{}` for `{}`: {error}",
                            directive.meta.name,
                            directive.owner.name()
                        )
                    })?;
                    let mut names = BTreeSet::new();
                    for task in &bundle.tasks {
                        if task.name.is_empty() || !names.insert(task.name) {
                            return Err(format!(
                                "`{}` has empty or duplicate background task names",
                                directive.meta.name
                            ));
                        }
                    }
                    if prepared.tasks.len() + bundle.tasks.len() > PluginTasks::LIMIT {
                        return Err("generation plugin task budget exceeded".into());
                    }
                    prepared.tasks.extend(bundle.tasks);
                    prepared
                        .handlers
                        .insert(directive.owner.clone(), (identity, bundle.handler));
                }
            }
        }
        prepared.secrets.seal();
        Ok(prepared)
    }
    pub fn handler(&self, instance: &DirectiveInstance) -> Result<Arc<dyn Handler>, String> {
        self.handlers
            .get(&instance.owner)
            .map(|(_, handler)| handler.clone())
            .ok_or_else(|| format!("`{}` was not prepared", instance.meta.name))
    }
    pub fn tasks(self, resources: &Resources<'_>) -> Result<PluginTasks, String> {
        PluginTasks::managed(self.tasks, resources.plugin_retirements)
    }
}
