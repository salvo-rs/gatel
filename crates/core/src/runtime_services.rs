use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::sync::RwLock;
use std::time::Duration;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};

use crate::config::{
    AppConfig, HandlerConfig, HoopConfig, LbPolicy, ProxyConfig, RouteConfig, SiteConfig,
    SiteTlsConfig, UpstreamConfig,
};
use crate::router::matcher::{RequestMatcher, pattern_specificity};

const RUNTIME_STATE_VERSION: u32 = 1;
pub const DEFAULT_RUNTIME_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RuntimeServiceError {
    #[error("service id must not be empty")]
    EmptyServiceId,

    #[error("service '{0}' was not found")]
    UnknownService(String),

    #[error("listener id must not be empty")]
    EmptyListenerId,

    #[error("listener '{listener_id}' was not found in service '{service_id}'")]
    UnknownListener {
        service_id: String,
        listener_id: String,
    },

    #[error("listener id '{0}' is duplicated")]
    DuplicateListenerId(String),

    #[error("listener protocol '{0}' is duplicated")]
    DuplicateListenerProtocol(String),

    #[error("route id must not be empty")]
    EmptyRouteId,

    #[error("route '{route_id}' was not found in service '{service_id}'")]
    UnknownRoute {
        service_id: String,
        route_id: String,
    },

    #[error("route id '{0}' is duplicated")]
    DuplicateRouteId(String),

    #[error("route path prefix must start with '/'")]
    InvalidPathPrefix,

    #[error("target group id must not be empty")]
    EmptyTargetGroupId,

    #[error("target group '{group_id}' was not found in service '{service_id}' route '{route_id}'")]
    UnknownTargetGroup {
        service_id: String,
        route_id: String,
        group_id: String,
    },

    #[error("target group id '{0}' is duplicated")]
    DuplicateTargetGroupId(String),

    #[error("target id must not be empty")]
    EmptyTargetId,

    #[error(
        "target '{target_id}' was not found in service '{service_id}' route '{route_id}' group '{group_id}'"
    )]
    UnknownTarget {
        service_id: String,
        route_id: String,
        group_id: String,
        target_id: String,
    },

    #[error("target id '{0}' is duplicated")]
    DuplicateTargetId(String),

    #[error("target address for '{0}' must not be empty")]
    EmptyTargetAddress(String),

    #[error("expected revision {expected}, but current revision is {actual}")]
    RevisionConflict { expected: u64, actual: u64 },

    #[error(
        "route conflicts with existing service '{existing_service_id}' route '{existing_route_id}' on host '{host}' path '{path}' protocol '{protocol}'"
    )]
    RouteConflict {
        existing_service_id: String,
        existing_route_id: String,
        host: String,
        path: String,
        protocol: String,
    },

    #[error("runtime state version {0} is not supported")]
    UnsupportedVersion(u32),

    #[error("runtime TLS certificate and private key refs must both be set")]
    IncompleteTlsMaterialRefs,

    #[error("runtime TLS with manual certificate refs is not supported for wildcard hosts")]
    WildcardTlsHost,

    #[error(
        "canonical host '{canonical_host}' must be explicitly declared on service '{service_id}'"
    )]
    UnknownCanonicalHost {
        service_id: String,
        canonical_host: String,
    },

    #[error("canonical host '{0}' is invalid")]
    InvalidCanonicalHost(String),

    #[error(
        "TLS host '{host}' conflicts between service '{existing_service_id}' and service '{service_id}'"
    )]
    TlsHostConflict {
        host: String,
        existing_service_id: String,
        service_id: String,
    },

    #[error("TLS host '{host}' conflicts between static config and service '{service_id}'")]
    StaticTlsHostConflict { host: String, service_id: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Default)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeListenerProtocol {
    #[default]
    Http,
    Https,
}

impl RuntimeListenerProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeListener {
    pub id: String,
    #[serde(default)]
    pub protocol: RuntimeListenerProtocol,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeTargetState {
    #[default]
    Warming,
    Active,
    Draining,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct RuntimeTlsPolicy {
    #[serde(default)]
    pub certificate_ref: Option<String>,
    #[serde(default)]
    pub private_key_ref: Option<String>,
    #[serde(default)]
    pub https_redirect: Option<bool>,
    #[serde(default)]
    pub canonical_host: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeHealthCheck {
    #[serde(default = "default_health_path")]
    pub path: String,
    #[serde(
        default = "default_health_interval",
        serialize_with = "serialize_duration",
        deserialize_with = "deserialize_duration"
    )]
    pub interval: Duration,
    #[serde(
        default = "default_health_timeout",
        serialize_with = "serialize_duration",
        deserialize_with = "deserialize_duration"
    )]
    pub timeout: Duration,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default = "default_success_threshold")]
    pub success_threshold: u32,
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,
}

impl Default for RuntimeHealthCheck {
    fn default() -> Self {
        Self {
            path: default_health_path(),
            interval: default_health_interval(),
            timeout: default_health_timeout(),
            port: None,
            host: None,
            success_threshold: default_success_threshold(),
            failure_threshold: default_failure_threshold(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeTarget {
    pub id: String,
    pub addr: String,
    #[serde(default = "default_target_weight")]
    pub weight: u32,
    #[serde(default)]
    pub state: RuntimeTargetState,
    #[serde(
        default = "default_drain_timeout",
        serialize_with = "serialize_duration",
        deserialize_with = "deserialize_duration"
    )]
    pub drain_timeout: Duration,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeTargetGroup {
    pub id: String,
    #[serde(default = "default_group_weight")]
    pub weight: u32,
    #[serde(default)]
    pub targets: Vec<RuntimeTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeRoute {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default = "default_path_prefix")]
    pub path_prefix: String,
    #[serde(default)]
    pub matchers: Vec<RequestMatcher>,
    #[serde(default = "default_strip_path_prefix")]
    pub strip_path_prefix: bool,
    #[serde(default = "default_lb_policy")]
    pub lb: LbPolicy,
    #[serde(default)]
    pub lb_header: Option<String>,
    #[serde(default)]
    pub lb_cookie: Option<String>,
    #[serde(default)]
    pub target_groups: Vec<RuntimeTargetGroup>,
    #[serde(default)]
    pub health_check: RuntimeHealthCheck,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeService {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub listeners: Vec<RuntimeListener>,
    #[serde(default)]
    pub routes: Vec<RuntimeRoute>,
    #[serde(default)]
    pub tls: Option<RuntimeTlsPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeState {
    #[serde(default = "default_state_version")]
    pub version: u32,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub services: BTreeMap<String, RuntimeService>,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            version: RUNTIME_STATE_VERSION,
            revision: 0,
            services: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RuntimeServicePatch {
    #[serde(default)]
    pub listeners: Option<Vec<RuntimeListener>>,
    #[serde(default)]
    pub tls: Option<RuntimeTlsPolicy>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RuntimeRoutePatch {
    #[serde(default)]
    pub hosts: Option<Vec<String>>,
    #[serde(default)]
    pub path_prefix: Option<String>,
    #[serde(default)]
    pub matchers: Option<Vec<RequestMatcher>>,
    #[serde(default)]
    pub strip_path_prefix: Option<bool>,
    #[serde(default)]
    pub lb: Option<LbPolicy>,
    #[serde(default)]
    pub lb_header: Option<String>,
    #[serde(default)]
    pub lb_cookie: Option<String>,
    #[serde(default)]
    pub target_groups: Option<Vec<RuntimeTargetGroup>>,
    #[serde(default)]
    pub health_check: Option<RuntimeHealthCheck>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RuntimeListenerPatch {
    #[serde(default)]
    pub protocol: Option<RuntimeListenerProtocol>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RuntimeTargetPatch {
    #[serde(default)]
    pub addr: Option<String>,
    #[serde(default)]
    pub weight: Option<u32>,
    #[serde(default)]
    pub state: Option<RuntimeTargetState>,
    #[serde(default, deserialize_with = "deserialize_optional_duration")]
    pub drain_timeout: Option<Duration>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RuntimeTargetRef {
    pub group_id: String,
    pub target: RuntimeTarget,
}

#[derive(Debug, Default)]
pub struct RuntimeServiceRegistry {
    state: RwLock<RuntimeState>,
}

impl RuntimeServiceRegistry {
    pub fn snapshot(&self) -> RuntimeState {
        self.state
            .read()
            .expect("runtime service registry poisoned")
            .clone()
    }

    pub fn replace(&self, state: RuntimeState) {
        *self
            .state
            .write()
            .expect("runtime service registry poisoned") = state;
    }

    pub fn list(&self) -> Vec<RuntimeService> {
        self.snapshot().list_services()
    }

    pub fn get(&self, id: &str) -> Option<RuntimeService> {
        self.snapshot().get_service(id)
    }
}

impl RuntimeState {
    pub fn ensure_supported_version(&self) -> Result<(), RuntimeServiceError> {
        if self.version != RUNTIME_STATE_VERSION {
            return Err(RuntimeServiceError::UnsupportedVersion(self.version));
        }
        Ok(())
    }

    pub fn list_services(&self) -> Vec<RuntimeService> {
        self.services.values().cloned().collect()
    }

    pub fn get_service(&self, service_id: &str) -> Option<RuntimeService> {
        self.services.get(service_id).cloned()
    }

    pub fn list_listeners(
        &self,
        service_id: &str,
    ) -> Result<Vec<RuntimeListener>, RuntimeServiceError> {
        Ok(self
            .services
            .get(service_id)
            .ok_or_else(|| RuntimeServiceError::UnknownService(service_id.to_string()))?
            .listeners
            .clone())
    }

    pub fn get_listener(
        &self,
        service_id: &str,
        listener_id: &str,
    ) -> Result<RuntimeListener, RuntimeServiceError> {
        self.services
            .get(service_id)
            .ok_or_else(|| RuntimeServiceError::UnknownService(service_id.to_string()))?
            .listeners
            .iter()
            .find(|listener| listener.id == listener_id)
            .cloned()
            .ok_or_else(|| RuntimeServiceError::UnknownListener {
                service_id: service_id.to_string(),
                listener_id: listener_id.to_string(),
            })
    }

    pub fn list_routes(&self, service_id: &str) -> Result<Vec<RuntimeRoute>, RuntimeServiceError> {
        Ok(self
            .services
            .get(service_id)
            .ok_or_else(|| RuntimeServiceError::UnknownService(service_id.to_string()))?
            .routes
            .clone())
    }

    pub fn get_route(
        &self,
        service_id: &str,
        route_id: &str,
    ) -> Result<RuntimeRoute, RuntimeServiceError> {
        self.services
            .get(service_id)
            .ok_or_else(|| RuntimeServiceError::UnknownService(service_id.to_string()))?
            .routes
            .iter()
            .find(|route| route.id == route_id)
            .cloned()
            .ok_or_else(|| RuntimeServiceError::UnknownRoute {
                service_id: service_id.to_string(),
                route_id: route_id.to_string(),
            })
    }

    pub fn list_targets(
        &self,
        service_id: &str,
        route_id: &str,
        group_id: &str,
    ) -> Result<Vec<RuntimeTarget>, RuntimeServiceError> {
        Ok(self
            .find_group(service_id, route_id, group_id)?
            .targets
            .clone())
    }

    pub fn list_route_targets(
        &self,
        service_id: &str,
        route_id: &str,
    ) -> Result<Vec<RuntimeTargetRef>, RuntimeServiceError> {
        let route = self.get_route(service_id, route_id)?;
        let mut targets = Vec::new();
        for group in route.target_groups {
            for target in group.targets {
                targets.push(RuntimeTargetRef {
                    group_id: group.id.clone(),
                    target,
                });
            }
        }
        Ok(targets)
    }

    pub fn get_target(
        &self,
        service_id: &str,
        route_id: &str,
        group_id: &str,
        target_id: &str,
    ) -> Result<RuntimeTarget, RuntimeServiceError> {
        self.find_group(service_id, route_id, group_id)?
            .targets
            .iter()
            .find(|target| target.id == target_id)
            .cloned()
            .ok_or_else(|| RuntimeServiceError::UnknownTarget {
                service_id: service_id.to_string(),
                route_id: route_id.to_string(),
                group_id: group_id.to_string(),
                target_id: target_id.to_string(),
            })
    }

    pub fn upsert_service(
        &mut self,
        mut service: RuntimeService,
        expected_revision: Option<u64>,
    ) -> Result<RuntimeService, RuntimeServiceError> {
        normalize_service(&mut service)?;

        let current_revision = self
            .services
            .get(&service.id)
            .map(|existing| existing.revision)
            .unwrap_or(0);
        check_revision(expected_revision, current_revision)?;

        let mut next = self.clone();
        let revision = next.next_revision();
        service.revision = revision;
        next.services.insert(service.id.clone(), service.clone());
        validate_state(&next)?;

        *self = next;
        Ok(service)
    }

    pub fn patch_service(
        &mut self,
        service_id: &str,
        patch: RuntimeServicePatch,
        expected_revision: Option<u64>,
    ) -> Result<RuntimeService, RuntimeServiceError> {
        let current = self
            .services
            .get(service_id)
            .cloned()
            .ok_or_else(|| RuntimeServiceError::UnknownService(service_id.to_string()))?;
        check_revision(expected_revision, current.revision)?;

        let current_revision = current.revision;
        let mut service = current;
        if let Some(listeners) = patch.listeners {
            service.listeners = listeners;
        }
        if let Some(tls) = patch.tls {
            service.tls = Some(tls);
        }

        self.upsert_service(service, Some(current_revision))
    }

    pub fn remove_service(
        &mut self,
        service_id: &str,
        expected_revision: Option<u64>,
    ) -> Result<RuntimeService, RuntimeServiceError> {
        let current = self
            .services
            .get(service_id)
            .cloned()
            .ok_or_else(|| RuntimeServiceError::UnknownService(service_id.to_string()))?;
        check_revision(expected_revision, current.revision)?;

        let mut next = self.clone();
        let removed = next
            .services
            .remove(service_id)
            .ok_or_else(|| RuntimeServiceError::UnknownService(service_id.to_string()))?;
        next.next_revision();

        *self = next;
        Ok(removed)
    }

    pub fn upsert_listener(
        &mut self,
        service_id: &str,
        listener_id: &str,
        mut listener: RuntimeListener,
        expected_revision: Option<u64>,
    ) -> Result<RuntimeListener, RuntimeServiceError> {
        listener.id = listener_id.to_string();
        normalize_listener(&mut listener)?;

        let mut next = self.clone();
        let service = next
            .services
            .get_mut(service_id)
            .ok_or_else(|| RuntimeServiceError::UnknownService(service_id.to_string()))?;
        check_revision(expected_revision, service.revision)?;

        if let Some(existing) = service
            .listeners
            .iter_mut()
            .find(|existing| existing.id == listener.id)
        {
            *existing = listener.clone();
        } else {
            service.listeners.push(listener.clone());
        }

        sort_service(service);
        let revision = next.next_revision();
        next.services
            .get_mut(service_id)
            .expect("service exists")
            .revision = revision;
        validate_state(&next)?;

        *self = next;
        Ok(listener)
    }

    pub fn patch_listener(
        &mut self,
        service_id: &str,
        listener_id: &str,
        patch: RuntimeListenerPatch,
        expected_revision: Option<u64>,
    ) -> Result<RuntimeListener, RuntimeServiceError> {
        let mut listener = self.get_listener(service_id, listener_id)?;
        if let Some(protocol) = patch.protocol {
            listener.protocol = protocol;
        }
        self.upsert_listener(service_id, listener_id, listener, expected_revision)
    }

    pub fn remove_listener(
        &mut self,
        service_id: &str,
        listener_id: &str,
        expected_revision: Option<u64>,
    ) -> Result<RuntimeListener, RuntimeServiceError> {
        let mut next = self.clone();
        let service = next
            .services
            .get_mut(service_id)
            .ok_or_else(|| RuntimeServiceError::UnknownService(service_id.to_string()))?;
        check_revision(expected_revision, service.revision)?;

        let index = service
            .listeners
            .iter()
            .position(|listener| listener.id == listener_id)
            .ok_or_else(|| RuntimeServiceError::UnknownListener {
                service_id: service_id.to_string(),
                listener_id: listener_id.to_string(),
            })?;

        let removed = service.listeners.remove(index);
        let revision = next.next_revision();
        next.services
            .get_mut(service_id)
            .expect("service exists")
            .revision = revision;
        validate_state(&next)?;

        *self = next;
        Ok(removed)
    }

    pub fn upsert_route(
        &mut self,
        service_id: &str,
        route_id: &str,
        mut route: RuntimeRoute,
        expected_revision: Option<u64>,
    ) -> Result<RuntimeRoute, RuntimeServiceError> {
        route.id = route_id.to_string();
        normalize_route(&mut route)?;

        let mut next = self.clone();
        let service = next
            .services
            .get_mut(service_id)
            .ok_or_else(|| RuntimeServiceError::UnknownService(service_id.to_string()))?;
        check_revision(expected_revision, service.revision)?;

        if let Some(existing) = service
            .routes
            .iter_mut()
            .find(|existing| existing.id == route.id)
        {
            *existing = route.clone();
        } else {
            service.routes.push(route.clone());
        }

        sort_service(service);
        let revision = next.next_revision();
        next.services
            .get_mut(service_id)
            .expect("service exists")
            .revision = revision;
        validate_state(&next)?;

        *self = next;
        Ok(route)
    }

    pub fn patch_route(
        &mut self,
        service_id: &str,
        route_id: &str,
        patch: RuntimeRoutePatch,
        expected_revision: Option<u64>,
    ) -> Result<RuntimeRoute, RuntimeServiceError> {
        let mut route = self.get_route(service_id, route_id)?;
        if let Some(hosts) = patch.hosts {
            route.hosts = hosts;
        }
        if let Some(path_prefix) = patch.path_prefix {
            route.path_prefix = path_prefix;
        }
        if let Some(matchers) = patch.matchers {
            route.matchers = matchers;
        }
        if let Some(strip_path_prefix) = patch.strip_path_prefix {
            route.strip_path_prefix = strip_path_prefix;
        }
        if let Some(lb) = patch.lb {
            route.lb = lb;
        }
        if let Some(lb_header) = patch.lb_header {
            route.lb_header = Some(lb_header);
        }
        if let Some(lb_cookie) = patch.lb_cookie {
            route.lb_cookie = Some(lb_cookie);
        }
        if let Some(target_groups) = patch.target_groups {
            route.target_groups = target_groups;
        }
        if let Some(health_check) = patch.health_check {
            route.health_check = health_check;
        }
        self.upsert_route(service_id, route_id, route, expected_revision)
    }

    pub fn remove_route(
        &mut self,
        service_id: &str,
        route_id: &str,
        expected_revision: Option<u64>,
    ) -> Result<RuntimeRoute, RuntimeServiceError> {
        let mut next = self.clone();
        let service = next
            .services
            .get_mut(service_id)
            .ok_or_else(|| RuntimeServiceError::UnknownService(service_id.to_string()))?;
        check_revision(expected_revision, service.revision)?;

        let index = service
            .routes
            .iter()
            .position(|route| route.id == route_id)
            .ok_or_else(|| RuntimeServiceError::UnknownRoute {
                service_id: service_id.to_string(),
                route_id: route_id.to_string(),
            })?;

        let removed = service.routes.remove(index);
        let revision = next.next_revision();
        next.services
            .get_mut(service_id)
            .expect("service exists")
            .revision = revision;
        validate_state(&next)?;

        *self = next;
        Ok(removed)
    }

    pub fn upsert_target(
        &mut self,
        service_id: &str,
        route_id: &str,
        group_id: &str,
        target_id: &str,
        mut target: RuntimeTarget,
        expected_revision: Option<u64>,
    ) -> Result<RuntimeTarget, RuntimeServiceError> {
        target.id = target_id.to_string();
        normalize_target(&mut target)?;

        let mut next = self.clone();
        let service = next
            .services
            .get_mut(service_id)
            .ok_or_else(|| RuntimeServiceError::UnknownService(service_id.to_string()))?;
        check_revision(expected_revision, service.revision)?;

        let route =
            find_route_mut(service, route_id).ok_or_else(|| RuntimeServiceError::UnknownRoute {
                service_id: service_id.to_string(),
                route_id: route_id.to_string(),
            })?;
        let group = find_group_mut(route, group_id).ok_or_else(|| {
            RuntimeServiceError::UnknownTargetGroup {
                service_id: service_id.to_string(),
                route_id: route_id.to_string(),
                group_id: group_id.to_string(),
            }
        })?;

        if let Some(existing) = group
            .targets
            .iter_mut()
            .find(|existing| existing.id == target.id)
        {
            *existing = target.clone();
        } else {
            group.targets.push(target.clone());
        }

        sort_route(route);
        sort_service(service);
        let revision = next.next_revision();
        next.services
            .get_mut(service_id)
            .expect("service exists")
            .revision = revision;
        validate_state(&next)?;

        *self = next;
        Ok(target)
    }

    pub fn patch_target(
        &mut self,
        service_id: &str,
        route_id: &str,
        group_id: &str,
        target_id: &str,
        patch: RuntimeTargetPatch,
        expected_revision: Option<u64>,
    ) -> Result<RuntimeTarget, RuntimeServiceError> {
        let mut target = self.get_target(service_id, route_id, group_id, target_id)?;
        if let Some(addr) = patch.addr {
            target.addr = addr;
        }
        if let Some(weight) = patch.weight {
            target.weight = weight;
        }
        if let Some(state) = patch.state {
            target.state = state;
            if target.state != RuntimeTargetState::Failed {
                target.last_error = None;
            }
        }
        if let Some(drain_timeout) = patch.drain_timeout {
            target.drain_timeout = drain_timeout;
        }
        self.upsert_target(
            service_id,
            route_id,
            group_id,
            target_id,
            target,
            expected_revision,
        )
    }

    pub fn remove_target(
        &mut self,
        service_id: &str,
        route_id: &str,
        group_id: &str,
        target_id: &str,
        expected_revision: Option<u64>,
    ) -> Result<RuntimeTarget, RuntimeServiceError> {
        let mut next = self.clone();
        let service = next
            .services
            .get_mut(service_id)
            .ok_or_else(|| RuntimeServiceError::UnknownService(service_id.to_string()))?;
        check_revision(expected_revision, service.revision)?;

        let route =
            find_route_mut(service, route_id).ok_or_else(|| RuntimeServiceError::UnknownRoute {
                service_id: service_id.to_string(),
                route_id: route_id.to_string(),
            })?;
        let group = find_group_mut(route, group_id).ok_or_else(|| {
            RuntimeServiceError::UnknownTargetGroup {
                service_id: service_id.to_string(),
                route_id: route_id.to_string(),
                group_id: group_id.to_string(),
            }
        })?;
        let index = group
            .targets
            .iter()
            .position(|target| target.id == target_id)
            .ok_or_else(|| RuntimeServiceError::UnknownTarget {
                service_id: service_id.to_string(),
                route_id: route_id.to_string(),
                group_id: group_id.to_string(),
                target_id: target_id.to_string(),
            })?;

        let removed = group.targets.remove(index);
        sort_route(route);
        sort_service(service);
        let revision = next.next_revision();
        next.services
            .get_mut(service_id)
            .expect("service exists")
            .revision = revision;
        validate_state(&next)?;

        *self = next;
        Ok(removed)
    }

    pub fn set_target_state(
        &mut self,
        service_id: &str,
        route_id: &str,
        group_id: &str,
        target_id: &str,
        state: RuntimeTargetState,
        last_error: Option<String>,
    ) -> Result<RuntimeTarget, RuntimeServiceError> {
        let mut next = self.clone();
        let service = next
            .services
            .get_mut(service_id)
            .ok_or_else(|| RuntimeServiceError::UnknownService(service_id.to_string()))?;
        let route =
            find_route_mut(service, route_id).ok_or_else(|| RuntimeServiceError::UnknownRoute {
                service_id: service_id.to_string(),
                route_id: route_id.to_string(),
            })?;
        let group = find_group_mut(route, group_id).ok_or_else(|| {
            RuntimeServiceError::UnknownTargetGroup {
                service_id: service_id.to_string(),
                route_id: route_id.to_string(),
                group_id: group_id.to_string(),
            }
        })?;
        let target = find_target_mut(group, target_id).ok_or_else(|| {
            RuntimeServiceError::UnknownTarget {
                service_id: service_id.to_string(),
                route_id: route_id.to_string(),
                group_id: group_id.to_string(),
                target_id: target_id.to_string(),
            }
        })?;

        target.state = state;
        target.last_error = match target.state {
            RuntimeTargetState::Failed => last_error,
            _ => None,
        };

        let revision = next.next_revision();
        next.services
            .get_mut(service_id)
            .expect("service exists")
            .revision = revision;
        validate_state(&next)?;
        let updated = next.get_target(service_id, route_id, group_id, target_id)?;

        *self = next;
        Ok(updated)
    }

    fn find_group(
        &self,
        service_id: &str,
        route_id: &str,
        group_id: &str,
    ) -> Result<RuntimeTargetGroup, RuntimeServiceError> {
        self.get_route(service_id, route_id)?
            .target_groups
            .iter()
            .find(|group| group.id == group_id)
            .cloned()
            .ok_or_else(|| RuntimeServiceError::UnknownTargetGroup {
                service_id: service_id.to_string(),
                route_id: route_id.to_string(),
                group_id: group_id.to_string(),
            })
    }

    fn next_revision(&mut self) -> u64 {
        self.revision = self.revision.saturating_add(1);
        self.revision
    }
}

pub fn merge_runtime_config(
    base_config: &AppConfig,
    runtime_state: &RuntimeState,
) -> Result<AppConfig, RuntimeServiceError> {
    runtime_state.ensure_supported_version()?;
    validate_state(runtime_state)?;

    let mut merged = base_config.clone();
    let mut runtime_sites: BTreeMap<String, RuntimeMergedSite> = BTreeMap::new();

    for service in runtime_state.services.values() {
        let listener_protocols = listener_protocols_for_service(service);
        let site_tls = service_site_tls_config(service);
        let canonical_host = service
            .tls
            .as_ref()
            .and_then(|tls| tls.canonical_host.as_deref());
        for route in &service.routes {
            let upstreams = active_upstreams(route);
            if upstreams.is_empty() {
                continue;
            }

            for host in normalized_route_hosts(route) {
                let host_key = if host == "*" {
                    "*".to_string()
                } else {
                    host.clone()
                };
                let route_config = if canonical_host.is_some_and(|canonical| host != canonical) {
                    canonical_redirect_route_config(service, route, &listener_protocols)
                } else {
                    proxy_route_config(service, route, upstreams.clone(), &listener_protocols)
                };
                upsert_runtime_site(
                    &mut runtime_sites,
                    host_key,
                    &service.id,
                    site_tls.as_ref(),
                    route_config,
                );
            }
        }
    }

    for runtime_site in runtime_sites.into_values() {
        let RuntimeMergedSite {
            site: runtime_site,
            owner_service_id,
        } = runtime_site;
        if let Some(existing) = merged
            .sites
            .iter_mut()
            .find(|site| site.host.eq_ignore_ascii_case(&runtime_site.host))
        {
            if let (Some(existing_tls), Some(runtime_tls)) = (&existing.tls, &runtime_site.tls)
                && existing_tls != runtime_tls
            {
                return Err(RuntimeServiceError::StaticTlsHostConflict {
                    host: runtime_site.host.clone(),
                    service_id: owner_service_id,
                });
            }

            if existing.tls.is_none() {
                existing.tls = runtime_site.tls.clone();
            }
            let mut routes = runtime_site.routes;
            routes.extend(existing.routes.clone());
            existing.routes = routes;
        } else {
            insert_runtime_site(&mut merged.sites, runtime_site);
        }
    }

    Ok(merged)
}

pub fn route_health_check(
    state: &RuntimeState,
    service_id: &str,
    route_id: &str,
) -> Result<RuntimeHealthCheck, RuntimeServiceError> {
    Ok(state.get_route(service_id, route_id)?.health_check)
}

fn validate_state(state: &RuntimeState) -> Result<(), RuntimeServiceError> {
    state.ensure_supported_version()?;

    let mut bindings = Vec::new();
    let mut tls_bindings: BTreeMap<String, RuntimeTlsBinding> = BTreeMap::new();

    for service in state.services.values() {
        validate_service_shape(service)?;
        let listener_protocols = listener_protocols_for_service(service);
        if let Some(site_tls) = service_site_tls_config(service) {
            for host in explicit_service_hosts(service) {
                if let Some(existing) = tls_bindings.get(&host) {
                    if existing.service_id != service.id && existing.site_tls != site_tls {
                        return Err(RuntimeServiceError::TlsHostConflict {
                            host,
                            existing_service_id: existing.service_id.clone(),
                            service_id: service.id.clone(),
                        });
                    }
                } else {
                    tls_bindings.insert(
                        host,
                        RuntimeTlsBinding {
                            service_id: service.id.clone(),
                            site_tls: site_tls.clone(),
                        },
                    );
                }
            }
        }

        for route in &service.routes {
            validate_route_shape(route)?;
            for host in normalized_route_hosts(route) {
                for protocol in &listener_protocols {
                    bindings.push(RouteBinding {
                        service_id: service.id.clone(),
                        route_id: route.id.clone(),
                        host: host.clone(),
                        path_prefix: route.path_prefix.clone(),
                        protocol: *protocol,
                        matcher_signatures: matcher_signatures(&route.matchers),
                    });
                }
            }
        }
    }

    for (index, binding) in bindings.iter().enumerate() {
        for other in bindings.iter().skip(index + 1) {
            if routes_conflict(binding, other) {
                return Err(RuntimeServiceError::RouteConflict {
                    existing_service_id: binding.service_id.clone(),
                    existing_route_id: binding.route_id.clone(),
                    host: canonical_conflict_host(&binding.host, &other.host),
                    path: other.path_prefix.clone(),
                    protocol: binding.protocol.as_str().to_string(),
                });
            }
        }
    }

    Ok(())
}

fn validate_service_shape(service: &RuntimeService) -> Result<(), RuntimeServiceError> {
    if service.id.trim().is_empty() {
        return Err(RuntimeServiceError::EmptyServiceId);
    }

    let mut listener_ids = HashSet::new();
    let mut listener_protocols = HashSet::new();
    for listener in &service.listeners {
        if listener.id.trim().is_empty() {
            return Err(RuntimeServiceError::EmptyListenerId);
        }
        if !listener_ids.insert(listener.id.clone()) {
            return Err(RuntimeServiceError::DuplicateListenerId(
                listener.id.clone(),
            ));
        }
        if !listener_protocols.insert(listener.protocol.as_str().to_string()) {
            return Err(RuntimeServiceError::DuplicateListenerProtocol(
                listener.protocol.as_str().to_string(),
            ));
        }
    }

    let mut route_ids = HashSet::new();
    for route in &service.routes {
        if route.id.trim().is_empty() {
            return Err(RuntimeServiceError::EmptyRouteId);
        }
        if !route_ids.insert(route.id.clone()) {
            return Err(RuntimeServiceError::DuplicateRouteId(route.id.clone()));
        }
    }

    if let Some(tls) = &service.tls {
        if matches!(
            (&tls.certificate_ref, &tls.private_key_ref),
            (Some(_), None) | (None, Some(_))
        ) {
            return Err(RuntimeServiceError::IncompleteTlsMaterialRefs);
        }

        let explicit_hosts = explicit_service_hosts(service);

        if tls.certificate_ref.is_some()
            && service
                .routes
                .iter()
                .any(|route| normalized_route_hosts(route).iter().any(|host| host == "*"))
        {
            return Err(RuntimeServiceError::WildcardTlsHost);
        }

        if let Some(canonical_host) = &tls.canonical_host
            && !explicit_hosts.contains(canonical_host)
        {
            return Err(RuntimeServiceError::UnknownCanonicalHost {
                service_id: service.id.clone(),
                canonical_host: canonical_host.clone(),
            });
        }
    }

    Ok(())
}

fn validate_route_shape(route: &RuntimeRoute) -> Result<(), RuntimeServiceError> {
    if route.id.trim().is_empty() {
        return Err(RuntimeServiceError::EmptyRouteId);
    }

    let mut group_ids = HashSet::new();
    let mut target_ids = HashSet::new();

    for group in &route.target_groups {
        if group.id.trim().is_empty() {
            return Err(RuntimeServiceError::EmptyTargetGroupId);
        }
        if !group_ids.insert(group.id.clone()) {
            return Err(RuntimeServiceError::DuplicateTargetGroupId(
                group.id.clone(),
            ));
        }

        for target in &group.targets {
            if target.id.trim().is_empty() {
                return Err(RuntimeServiceError::EmptyTargetId);
            }
            if !target_ids.insert(target.id.clone()) {
                return Err(RuntimeServiceError::DuplicateTargetId(target.id.clone()));
            }
            if target.addr.trim().is_empty() {
                return Err(RuntimeServiceError::EmptyTargetAddress(target.id.clone()));
            }
        }
    }

    Ok(())
}

fn listener_protocols_for_service(service: &RuntimeService) -> BTreeSet<RuntimeListenerProtocol> {
    if service.listeners.is_empty() {
        BTreeSet::from([
            RuntimeListenerProtocol::Http,
            RuntimeListenerProtocol::Https,
        ])
    } else {
        service
            .listeners
            .iter()
            .map(|listener| listener.protocol)
            .collect()
    }
}

fn active_upstreams(route: &RuntimeRoute) -> Vec<UpstreamConfig> {
    let mut upstreams = Vec::new();
    for group in &route.target_groups {
        for target in &group.targets {
            if target.state != RuntimeTargetState::Active {
                continue;
            }
            upstreams.push(UpstreamConfig {
                addr: target.addr.clone(),
                weight: group.weight.saturating_mul(target.weight).max(1),
            });
        }
    }
    upstreams
}

fn route_pattern(path_prefix: &str) -> String {
    if path_prefix == "/" {
        "/*".to_string()
    } else {
        format!("{path_prefix}/*")
    }
}

fn routes_conflict(left: &RouteBinding, right: &RouteBinding) -> bool {
    left.protocol == right.protocol
        && !(left.service_id == right.service_id && left.route_id == right.route_id)
        && left.path_prefix == right.path_prefix
        && conflicting_host_bindings(&left.host, &right.host)
        && left.matcher_signatures.len() == right.matcher_signatures.len()
}

fn normalized_route_hosts(route: &RuntimeRoute) -> Vec<String> {
    if route.hosts.is_empty() {
        vec!["*".to_string()]
    } else {
        route
            .hosts
            .iter()
            .map(|host| host.trim().to_ascii_lowercase())
            .filter(|host| !host.is_empty())
            .collect()
    }
}

fn conflicting_host_bindings(left: &str, right: &str) -> bool {
    (left == "*" && right == "*") || left.eq_ignore_ascii_case(right)
}

fn canonical_conflict_host(left: &str, right: &str) -> String {
    if left == "*" {
        right.to_string()
    } else {
        left.to_string()
    }
}

fn matcher_signatures(matchers: &[RequestMatcher]) -> Vec<String> {
    let mut signatures = matchers
        .iter()
        .map(|matcher| serde_json::to_string(matcher).unwrap_or_else(|_| format!("{matcher:?}")))
        .collect::<Vec<_>>();
    signatures.sort();
    signatures
}

fn compare_runtime_routes(left: &RuntimeRoute, right: &RuntimeRoute) -> Ordering {
    route_path_specificity(right)
        .cmp(&route_path_specificity(left))
        .then_with(|| route_host_specificity(right).cmp(&route_host_specificity(left)))
        .then_with(|| right.matchers.len().cmp(&left.matchers.len()))
        .then_with(|| matcher_signatures(&left.matchers).cmp(&matcher_signatures(&right.matchers)))
        .then_with(|| left.id.cmp(&right.id))
}

fn route_path_specificity(route: &RuntimeRoute) -> usize {
    pattern_specificity(&route_pattern(&route.path_prefix))
}

fn route_host_specificity(route: &RuntimeRoute) -> usize {
    normalized_route_hosts(route)
        .into_iter()
        .map(|host| if host == "*" { 0 } else { 1 })
        .max()
        .unwrap_or(0)
}

fn check_revision(expected: Option<u64>, actual: u64) -> Result<(), RuntimeServiceError> {
    if let Some(expected) = expected
        && expected != actual
    {
        return Err(RuntimeServiceError::RevisionConflict { expected, actual });
    }
    Ok(())
}

fn normalize_service(service: &mut RuntimeService) -> Result<(), RuntimeServiceError> {
    service.id = service.id.trim().to_string();
    if let Some(tls) = service.tls.as_mut() {
        normalize_tls_policy(tls)?;
    }
    for listener in &mut service.listeners {
        normalize_listener(listener)?;
    }
    for route in &mut service.routes {
        normalize_route(route)?;
    }
    sort_service(service);
    Ok(())
}

fn service_site_tls_config(service: &RuntimeService) -> Option<SiteTlsConfig> {
    service
        .tls
        .as_ref()
        .and_then(|tls| match (&tls.certificate_ref, &tls.private_key_ref) {
            (Some(cert), Some(key)) => Some(SiteTlsConfig {
                cert: cert.clone(),
                key: key.clone(),
            }),
            _ => None,
        })
}

fn explicit_service_hosts(service: &RuntimeService) -> BTreeSet<String> {
    service
        .routes
        .iter()
        .flat_map(normalized_route_hosts)
        .filter(|host| host != "*")
        .collect()
}

fn proxy_route_config(
    service: &RuntimeService,
    route: &RuntimeRoute,
    upstreams: Vec<UpstreamConfig>,
    listener_protocols: &BTreeSet<RuntimeListenerProtocol>,
) -> RouteConfig {
    let mut middlewares = Vec::new();
    if route.strip_path_prefix && route.path_prefix != "/" {
        middlewares.push(HoopConfig::Rewrite {
            strip_prefix: Some(route.path_prefix.clone()),
            uri: None,
            regex_rules: Vec::new(),
            if_not_file: false,
            if_not_dir: false,
            root: None,
            normalize_slashes: false,
        });
    }

    if service
        .tls
        .as_ref()
        .and_then(|tls| tls.https_redirect)
        .unwrap_or(false)
    {
        middlewares.push(HoopConfig::ForceHttps { https_port: None });
    }

    RouteConfig {
        path: route_pattern(&route.path_prefix),
        matchers: route_matchers_for_listener_protocols(route, listener_protocols),
        middlewares,
        handler: HandlerConfig::Proxy(ProxyConfig {
            upstreams,
            lb: route.lb,
            lb_header: route.lb_header.clone(),
            lb_cookie: route.lb_cookie.clone(),
            health_check: None,
            passive_health: None,
            headers_up: BTreeMap::new().into_iter().collect(),
            headers_down: BTreeMap::new().into_iter().collect(),
            retries: 0,
            dynamic_upstreams: None,
            error_pages: BTreeMap::new().into_iter().collect(),
            headers_up_replace: Vec::new(),
            tls_skip_verify: false,
            upstream_http2: false,
            max_connections: None,
            keepalive_timeout: None,
            sanitize_uri: true,
            srv_upstream: None,
        }),
        condition: None,
    }
}

fn canonical_redirect_route_config(
    service: &RuntimeService,
    route: &RuntimeRoute,
    listener_protocols: &BTreeSet<RuntimeListenerProtocol>,
) -> RouteConfig {
    let canonical_host = service
        .tls
        .as_ref()
        .and_then(|tls| tls.canonical_host.as_deref())
        .expect("canonical redirect requires canonical host");
    let to = if service
        .tls
        .as_ref()
        .and_then(|tls| tls.https_redirect)
        .unwrap_or(false)
    {
        format!("https://{canonical_host}{{path}}{{query_suffix}}")
    } else {
        format!("{{scheme}}://{canonical_host}{{path}}{{query_suffix}}")
    };

    RouteConfig {
        path: route_pattern(&route.path_prefix),
        matchers: route_matchers_for_listener_protocols(route, listener_protocols),
        middlewares: Vec::new(),
        handler: HandlerConfig::Redirect {
            to,
            permanent: true,
        },
        condition: None,
    }
}

fn route_matchers_for_listener_protocols(
    route: &RuntimeRoute,
    listener_protocols: &BTreeSet<RuntimeListenerProtocol>,
) -> Vec<RequestMatcher> {
    let mut matchers = route.matchers.clone();
    if listener_protocols.len() == 1 {
        let protocol = listener_protocols
            .iter()
            .next()
            .copied()
            .expect("single protocol exists");
        if !matchers.iter().any(|matcher| {
            matches!(
                matcher,
                RequestMatcher::Protocol(existing)
                    if existing.eq_ignore_ascii_case(protocol.as_str())
            )
        }) {
            matchers.push(RequestMatcher::Protocol(protocol.as_str().to_string()));
        }
    }
    matchers
}

fn normalize_tls_policy(tls: &mut RuntimeTlsPolicy) -> Result<(), RuntimeServiceError> {
    tls.certificate_ref = tls
        .certificate_ref
        .as_ref()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    tls.private_key_ref = tls
        .private_key_ref
        .as_ref()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    tls.canonical_host = tls
        .canonical_host
        .as_ref()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty());

    if matches!(
        (&tls.certificate_ref, &tls.private_key_ref),
        (Some(_), None) | (None, Some(_))
    ) {
        return Err(RuntimeServiceError::IncompleteTlsMaterialRefs);
    }
    if let Some(canonical_host) = &tls.canonical_host
        && canonical_host == "*"
    {
        return Err(RuntimeServiceError::InvalidCanonicalHost(
            canonical_host.clone(),
        ));
    }
    Ok(())
}

fn normalize_listener(listener: &mut RuntimeListener) -> Result<(), RuntimeServiceError> {
    listener.id = listener.id.trim().to_string();
    if listener.id.is_empty() {
        return Err(RuntimeServiceError::EmptyListenerId);
    }
    Ok(())
}

fn normalize_route(route: &mut RuntimeRoute) -> Result<(), RuntimeServiceError> {
    route.id = route.id.trim().to_string();
    if route.id.is_empty() {
        return Err(RuntimeServiceError::EmptyRouteId);
    }
    route.path_prefix = normalize_path_prefix(&route.path_prefix)?;
    route.hosts = route
        .hosts
        .iter()
        .map(|host| host.trim().to_ascii_lowercase())
        .filter(|host| !host.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    route.lb_header = route
        .lb_header
        .as_ref()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    route.lb_cookie = route
        .lb_cookie
        .as_ref()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    for group in &mut route.target_groups {
        normalize_group(group)?;
    }
    sort_route(route);
    Ok(())
}

fn normalize_group(group: &mut RuntimeTargetGroup) -> Result<(), RuntimeServiceError> {
    group.id = group.id.trim().to_string();
    if group.id.is_empty() {
        return Err(RuntimeServiceError::EmptyTargetGroupId);
    }
    for target in &mut group.targets {
        normalize_target(target)?;
    }
    group.targets.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(())
}

fn normalize_target(target: &mut RuntimeTarget) -> Result<(), RuntimeServiceError> {
    target.id = target.id.trim().to_string();
    if target.id.is_empty() {
        return Err(RuntimeServiceError::EmptyTargetId);
    }
    target.addr = target.addr.trim().to_string();
    if target.addr.is_empty() {
        return Err(RuntimeServiceError::EmptyTargetAddress(target.id.clone()));
    }
    if target.state != RuntimeTargetState::Failed {
        target.last_error = None;
    }
    Ok(())
}

fn normalize_path_prefix(path: &str) -> Result<String, RuntimeServiceError> {
    let trimmed = path.trim();
    if trimmed.is_empty() || !trimmed.starts_with('/') {
        return Err(RuntimeServiceError::InvalidPathPrefix);
    }
    if trimmed == "/" {
        Ok("/".to_string())
    } else {
        Ok(trimmed.trim_end_matches('/').to_string())
    }
}

fn sort_service(service: &mut RuntimeService) {
    service
        .listeners
        .sort_by(|left, right| left.id.cmp(&right.id));
    service.routes.sort_by(compare_runtime_routes);
    for route in &mut service.routes {
        sort_route(route);
    }
}

fn sort_route(route: &mut RuntimeRoute) {
    route
        .target_groups
        .sort_by(|left, right| left.id.cmp(&right.id));
    for group in &mut route.target_groups {
        group.targets.sort_by(|left, right| left.id.cmp(&right.id));
    }
}

fn find_route_mut<'a>(
    service: &'a mut RuntimeService,
    route_id: &str,
) -> Option<&'a mut RuntimeRoute> {
    service.routes.iter_mut().find(|route| route.id == route_id)
}

fn find_group_mut<'a>(
    route: &'a mut RuntimeRoute,
    group_id: &str,
) -> Option<&'a mut RuntimeTargetGroup> {
    route
        .target_groups
        .iter_mut()
        .find(|group| group.id == group_id)
}

fn find_target_mut<'a>(
    group: &'a mut RuntimeTargetGroup,
    target_id: &str,
) -> Option<&'a mut RuntimeTarget> {
    group
        .targets
        .iter_mut()
        .find(|target| target.id == target_id)
}

fn insert_runtime_site(sites: &mut Vec<SiteConfig>, runtime_site: SiteConfig) {
    if runtime_site.host == "*" {
        sites.push(runtime_site);
        return;
    }

    let insert_at = sites
        .iter()
        .position(|site| site.host == "*")
        .unwrap_or(sites.len());
    sites.insert(insert_at, runtime_site);
}

fn upsert_runtime_site(
    runtime_sites: &mut BTreeMap<String, RuntimeMergedSite>,
    host: String,
    service_id: &str,
    site_tls: Option<&SiteTlsConfig>,
    route: RouteConfig,
) {
    let entry = runtime_sites
        .entry(host.clone())
        .or_insert_with(|| RuntimeMergedSite {
            site: SiteConfig {
                host,
                tls: site_tls.cloned(),
                routes: Vec::new(),
            },
            owner_service_id: service_id.to_string(),
        });
    if entry.site.tls.is_none() && site_tls.is_some() {
        entry.site.tls = site_tls.cloned();
        entry.owner_service_id = service_id.to_string();
    }
    entry.site.routes.push(route);
}

fn default_state_version() -> u32 {
    RUNTIME_STATE_VERSION
}

fn default_path_prefix() -> String {
    "/".to_string()
}

fn default_strip_path_prefix() -> bool {
    true
}

fn default_lb_policy() -> LbPolicy {
    LbPolicy::WeightedRoundRobin
}

fn default_group_weight() -> u32 {
    100
}

fn default_target_weight() -> u32 {
    100
}

fn default_health_path() -> String {
    "/up".to_string()
}

fn default_health_interval() -> Duration {
    Duration::from_secs(1)
}

fn default_health_timeout() -> Duration {
    Duration::from_secs(5)
}

fn default_success_threshold() -> u32 {
    1
}

fn default_failure_threshold() -> u32 {
    3
}

fn default_drain_timeout() -> Duration {
    DEFAULT_RUNTIME_DRAIN_TIMEOUT
}

fn serialize_duration<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&format!("{}s", duration.as_secs()))
}

fn deserialize_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_duration_value(deserializer).map(|value| value.unwrap_or_default())
}

fn deserialize_optional_duration<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_duration_value(deserializer)
}

fn deserialize_duration_value<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
where
    D: Deserializer<'de>,
{
    struct DurationVisitor;

    impl<'de> Visitor<'de> for DurationVisitor {
        type Value = Option<Duration>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a duration string like '30s' or an integer number of seconds")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: Deserializer<'de>,
        {
            deserialize_duration_value(deserializer)
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Some(Duration::from_secs(value)))
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            if value < 0 {
                return Err(E::custom("duration must not be negative"));
            }
            Ok(Some(Duration::from_secs(value as u64)))
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            parse_duration(value).map(Some).map_err(E::custom)
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            self.visit_str(&value)
        }
    }

    deserializer.deserialize_any(DurationVisitor)
}

fn parse_duration(input: &str) -> Result<Duration, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("duration must not be empty".to_string());
    }

    let split_at = trimmed
        .find(|char: char| !char.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (number, suffix) = trimmed.split_at(split_at);
    let value = number
        .parse::<u64>()
        .map_err(|_| format!("invalid duration value '{input}'"))?;

    match suffix.trim() {
        "" | "s" => Ok(Duration::from_secs(value)),
        "ms" => Ok(Duration::from_millis(value)),
        "m" => Ok(Duration::from_secs(value.saturating_mul(60))),
        "h" => Ok(Duration::from_secs(value.saturating_mul(60 * 60))),
        other => Err(format!("unsupported duration suffix '{other}'")),
    }
}

#[derive(Debug)]
struct RouteBinding {
    service_id: String,
    route_id: String,
    host: String,
    path_prefix: String,
    protocol: RuntimeListenerProtocol,
    matcher_signatures: Vec<String>,
}

#[derive(Debug, Clone)]
struct RuntimeTlsBinding {
    service_id: String,
    site_tls: SiteTlsConfig,
}

#[derive(Debug, Clone)]
struct RuntimeMergedSite {
    site: SiteConfig,
    owner_service_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service_with_listener(
        id: &str,
        listener_id: &str,
        protocol: RuntimeListenerProtocol,
    ) -> RuntimeService {
        RuntimeService {
            id: id.to_string(),
            revision: 0,
            listeners: vec![RuntimeListener {
                id: listener_id.to_string(),
                protocol,
            }],
            routes: vec![RuntimeRoute {
                id: "api".to_string(),
                hosts: vec!["example.com".to_string()],
                path_prefix: "/api".to_string(),
                matchers: Vec::new(),
                strip_path_prefix: true,
                lb: default_lb_policy(),
                lb_header: None,
                lb_cookie: None,
                target_groups: vec![RuntimeTargetGroup {
                    id: "primary".to_string(),
                    weight: 100,
                    targets: vec![RuntimeTarget {
                        id: "app-1".to_string(),
                        addr: "127.0.0.1:3000".to_string(),
                        weight: 100,
                        state: RuntimeTargetState::Active,
                        drain_timeout: DEFAULT_RUNTIME_DRAIN_TIMEOUT,
                        last_error: None,
                    }],
                }],
                health_check: RuntimeHealthCheck::default(),
            }],
            tls: None,
        }
    }

    #[test]
    fn upsert_and_get_service() {
        let mut state = RuntimeState::default();
        let inserted = state
            .upsert_service(
                service_with_listener("api", "https", RuntimeListenerProtocol::Https),
                None,
            )
            .unwrap();

        assert_eq!(inserted.id, "api");
        assert_eq!(inserted.revision, 1);
        assert_eq!(
            state.get_service("api").unwrap().routes[0].path_prefix,
            "/api"
        );
    }

    #[test]
    fn rejects_overlapping_host_and_path_for_same_protocol() {
        let mut state = RuntimeState::default();
        state
            .upsert_service(
                service_with_listener("api", "https", RuntimeListenerProtocol::Https),
                None,
            )
            .unwrap();

        let error = state
            .upsert_service(
                service_with_listener("assets", "https", RuntimeListenerProtocol::Https),
                None,
            )
            .unwrap_err();

        assert_eq!(
            error,
            RuntimeServiceError::RouteConflict {
                existing_service_id: "api".to_string(),
                existing_route_id: "api".to_string(),
                host: "example.com".to_string(),
                path: "/api".to_string(),
                protocol: "https".to_string(),
            }
        );
    }

    #[test]
    fn allows_same_host_and_path_when_listener_protocol_differs() {
        let mut state = RuntimeState::default();
        state
            .upsert_service(
                service_with_listener("api-http", "http", RuntimeListenerProtocol::Http),
                None,
            )
            .unwrap();

        state
            .upsert_service(
                service_with_listener("api-https", "https", RuntimeListenerProtocol::Https),
                None,
            )
            .unwrap();

        assert_eq!(state.list_services().len(), 2);
    }

    #[test]
    fn allows_nested_path_routes_with_deterministic_precedence() {
        let mut service = service_with_listener("api", "https", RuntimeListenerProtocol::Https);
        service.routes.push(RuntimeRoute {
            id: "api-v2".to_string(),
            hosts: vec!["example.com".to_string()],
            path_prefix: "/api/v2".to_string(),
            matchers: Vec::new(),
            strip_path_prefix: true,
            lb: default_lb_policy(),
            lb_header: None,
            lb_cookie: None,
            target_groups: vec![RuntimeTargetGroup {
                id: "secondary".to_string(),
                weight: 100,
                targets: vec![RuntimeTarget {
                    id: "app-2".to_string(),
                    addr: "127.0.0.1:3001".to_string(),
                    weight: 100,
                    state: RuntimeTargetState::Active,
                    drain_timeout: DEFAULT_RUNTIME_DRAIN_TIMEOUT,
                    last_error: None,
                }],
            }],
            health_check: RuntimeHealthCheck::default(),
        });

        let mut state = RuntimeState::default();
        state.upsert_service(service, None).unwrap();

        let routes = state.list_routes("api").unwrap();
        assert_eq!(routes[0].id, "api-v2");
        assert_eq!(routes[1].id, "api");
    }

    #[test]
    fn allows_same_path_with_header_canary_and_fallback_routes() {
        let mut service = service_with_listener("api", "https", RuntimeListenerProtocol::Https);
        service.routes.push(RuntimeRoute {
            id: "api-canary".to_string(),
            hosts: vec!["example.com".to_string()],
            path_prefix: "/api".to_string(),
            matchers: vec![RequestMatcher::Header {
                name: "x-deploy".to_string(),
                pattern: "canary".to_string(),
            }],
            strip_path_prefix: true,
            lb: LbPolicy::CookieHash,
            lb_header: None,
            lb_cookie: Some("deploy".to_string()),
            target_groups: vec![RuntimeTargetGroup {
                id: "canary".to_string(),
                weight: 20,
                targets: vec![RuntimeTarget {
                    id: "app-2".to_string(),
                    addr: "127.0.0.1:3001".to_string(),
                    weight: 100,
                    state: RuntimeTargetState::Active,
                    drain_timeout: DEFAULT_RUNTIME_DRAIN_TIMEOUT,
                    last_error: None,
                }],
            }],
            health_check: RuntimeHealthCheck::default(),
        });

        let mut state = RuntimeState::default();
        state.upsert_service(service, None).unwrap();

        let routes = state.list_routes("api").unwrap();
        assert_eq!(routes[0].id, "api-canary");
        assert_eq!(routes[1].id, "api");
    }

    #[test]
    fn rejects_same_path_routes_with_peer_matcher_specificity() {
        let mut service = service_with_listener("api", "https", RuntimeListenerProtocol::Https);
        service.routes.push(RuntimeRoute {
            id: "api-cookie".to_string(),
            hosts: vec!["example.com".to_string()],
            path_prefix: "/api".to_string(),
            matchers: vec![RequestMatcher::Cookie {
                name: "deploy".to_string(),
                pattern: "canary".to_string(),
            }],
            strip_path_prefix: true,
            lb: default_lb_policy(),
            lb_header: None,
            lb_cookie: None,
            target_groups: vec![RuntimeTargetGroup {
                id: "cookie".to_string(),
                weight: 100,
                targets: vec![RuntimeTarget {
                    id: "app-2".to_string(),
                    addr: "127.0.0.1:3001".to_string(),
                    weight: 100,
                    state: RuntimeTargetState::Active,
                    drain_timeout: DEFAULT_RUNTIME_DRAIN_TIMEOUT,
                    last_error: None,
                }],
            }],
            health_check: RuntimeHealthCheck::default(),
        });
        service.routes.push(RuntimeRoute {
            id: "api-header".to_string(),
            hosts: vec!["example.com".to_string()],
            path_prefix: "/api".to_string(),
            matchers: vec![RequestMatcher::Header {
                name: "x-deploy".to_string(),
                pattern: "canary".to_string(),
            }],
            strip_path_prefix: true,
            lb: default_lb_policy(),
            lb_header: None,
            lb_cookie: None,
            target_groups: vec![RuntimeTargetGroup {
                id: "header".to_string(),
                weight: 100,
                targets: vec![RuntimeTarget {
                    id: "app-3".to_string(),
                    addr: "127.0.0.1:3002".to_string(),
                    weight: 100,
                    state: RuntimeTargetState::Active,
                    drain_timeout: DEFAULT_RUNTIME_DRAIN_TIMEOUT,
                    last_error: None,
                }],
            }],
            health_check: RuntimeHealthCheck::default(),
        });

        let mut state = RuntimeState::default();
        let error = state.upsert_service(service, None).unwrap_err();
        assert!(matches!(error, RuntimeServiceError::RouteConflict { .. }));
    }

    #[test]
    fn updates_nested_target_and_bumps_service_revision() {
        let mut state = RuntimeState::default();
        let service = state
            .upsert_service(
                service_with_listener("api", "https", RuntimeListenerProtocol::Https),
                None,
            )
            .unwrap();

        let target = state
            .patch_target(
                "api",
                "api",
                "primary",
                "app-1",
                RuntimeTargetPatch {
                    state: Some(RuntimeTargetState::Draining),
                    ..RuntimeTargetPatch::default()
                },
                Some(service.revision),
            )
            .unwrap();

        assert_eq!(target.state, RuntimeTargetState::Draining);
        assert_eq!(state.get_service("api").unwrap().revision, 2);
    }

    #[test]
    fn rejects_stale_revision() {
        let mut state = RuntimeState::default();
        let service = state
            .upsert_service(
                service_with_listener("api", "https", RuntimeListenerProtocol::Https),
                None,
            )
            .unwrap();

        let error = state
            .patch_service(
                "api",
                RuntimeServicePatch {
                    tls: Some(RuntimeTlsPolicy {
                        https_redirect: Some(true),
                        ..RuntimeTlsPolicy::default()
                    }),
                    ..RuntimeServicePatch::default()
                },
                Some(service.revision + 1),
            )
            .unwrap_err();

        assert_eq!(
            error,
            RuntimeServiceError::RevisionConflict {
                expected: service.revision + 1,
                actual: service.revision,
            }
        );
    }

    #[test]
    fn merges_active_runtime_routes_into_config() {
        let mut state = RuntimeState::default();
        state
            .upsert_service(
                RuntimeService {
                    id: "api".to_string(),
                    revision: 0,
                    listeners: vec![RuntimeListener {
                        id: "https".to_string(),
                        protocol: RuntimeListenerProtocol::Https,
                    }],
                    routes: vec![RuntimeRoute {
                        id: "api".to_string(),
                        hosts: vec!["example.com".to_string()],
                        path_prefix: "/api".to_string(),
                        matchers: vec![
                            RequestMatcher::Header {
                                name: "x-deploy".to_string(),
                                pattern: "canary".to_string(),
                            },
                            RequestMatcher::Cookie {
                                name: "deploy".to_string(),
                                pattern: "canary".to_string(),
                            },
                        ],
                        strip_path_prefix: true,
                        lb: LbPolicy::CookieHash,
                        lb_header: None,
                        lb_cookie: Some("deploy".to_string()),
                        target_groups: vec![RuntimeTargetGroup {
                            id: "primary".to_string(),
                            weight: 50,
                            targets: vec![
                                RuntimeTarget {
                                    id: "warming".to_string(),
                                    addr: "127.0.0.1:3001".to_string(),
                                    weight: 100,
                                    state: RuntimeTargetState::Warming,
                                    drain_timeout: DEFAULT_RUNTIME_DRAIN_TIMEOUT,
                                    last_error: None,
                                },
                                RuntimeTarget {
                                    id: "active".to_string(),
                                    addr: "127.0.0.1:3002".to_string(),
                                    weight: 100,
                                    state: RuntimeTargetState::Active,
                                    drain_timeout: DEFAULT_RUNTIME_DRAIN_TIMEOUT,
                                    last_error: None,
                                },
                            ],
                        }],
                        health_check: RuntimeHealthCheck::default(),
                    }],
                    tls: Some(RuntimeTlsPolicy {
                        https_redirect: Some(true),
                        ..RuntimeTlsPolicy::default()
                    }),
                },
                None,
            )
            .unwrap();

        let base = AppConfig::default();
        let merged = merge_runtime_config(&base, &state).unwrap();
        let site = merged
            .sites
            .iter()
            .find(|site| site.host == "example.com")
            .unwrap();
        let route = site
            .routes
            .iter()
            .find(|route| route.path == "/api/*")
            .unwrap();

        match &route.handler {
            HandlerConfig::Proxy(proxy) => {
                assert_eq!(proxy.upstreams.len(), 1);
                assert_eq!(proxy.upstreams[0].addr, "127.0.0.1:3002");
                assert_eq!(proxy.upstreams[0].weight, 5000);
                assert_eq!(proxy.lb, LbPolicy::CookieHash);
                assert_eq!(proxy.lb_cookie.as_deref(), Some("deploy"));
            }
            other => panic!("unexpected handler: {other:?}"),
        }

        assert!(
            route
                .matchers
                .iter()
                .any(|matcher| matches!(matcher, RequestMatcher::Header { name, pattern } if name == "x-deploy" && pattern == "canary"))
        );
        assert!(
            route
                .matchers
                .iter()
                .any(|matcher| matches!(matcher, RequestMatcher::Cookie { name, pattern } if name == "deploy" && pattern == "canary"))
        );
        assert!(route.matchers.iter().any(
            |matcher| matches!(matcher, RequestMatcher::Protocol(protocol) if protocol == "https")
        ));
        assert!(
            route
                .middlewares
                .iter()
                .any(|middleware| matches!(middleware, HoopConfig::ForceHttps { .. }))
        );
    }

    #[test]
    fn rejects_conflicting_runtime_tls_hosts() {
        let mut state = RuntimeState::default();

        let mut api = service_with_listener("api", "https", RuntimeListenerProtocol::Https);
        api.tls = Some(RuntimeTlsPolicy {
            certificate_ref: Some("api-cert.pem".to_string()),
            private_key_ref: Some("api-key.pem".to_string()),
            ..RuntimeTlsPolicy::default()
        });
        state.upsert_service(api, None).unwrap();

        let mut assets = service_with_listener("assets", "https", RuntimeListenerProtocol::Https);
        assets.routes[0].path_prefix = "/assets".to_string();
        assets.tls = Some(RuntimeTlsPolicy {
            certificate_ref: Some("assets-cert.pem".to_string()),
            private_key_ref: Some("assets-key.pem".to_string()),
            ..RuntimeTlsPolicy::default()
        });

        let error = state.upsert_service(assets, None).unwrap_err();
        assert_eq!(
            error,
            RuntimeServiceError::TlsHostConflict {
                host: "example.com".to_string(),
                existing_service_id: "api".to_string(),
                service_id: "assets".to_string(),
            }
        );
    }

    #[test]
    fn rejects_conflicting_static_tls_host_on_merge() {
        let mut service = service_with_listener("api", "https", RuntimeListenerProtocol::Https);
        service.tls = Some(RuntimeTlsPolicy {
            certificate_ref: Some("runtime-cert.pem".to_string()),
            private_key_ref: Some("runtime-key.pem".to_string()),
            ..RuntimeTlsPolicy::default()
        });

        let mut state = RuntimeState::default();
        state.upsert_service(service, None).unwrap();

        let mut base = AppConfig::default();
        base.sites.push(SiteConfig {
            host: "example.com".to_string(),
            tls: Some(SiteTlsConfig {
                cert: "static-cert.pem".to_string(),
                key: "static-key.pem".to_string(),
            }),
            routes: Vec::new(),
        });

        let error = merge_runtime_config(&base, &state).unwrap_err();
        assert_eq!(
            error,
            RuntimeServiceError::StaticTlsHostConflict {
                host: "example.com".to_string(),
                service_id: "api".to_string(),
            }
        );
    }

    #[test]
    fn merges_runtime_tls_and_canonical_host_policy() {
        let mut service = service_with_listener("api", "https", RuntimeListenerProtocol::Https);
        service.routes[0].hosts = vec!["example.com".to_string(), "www.example.com".to_string()];
        service.tls = Some(RuntimeTlsPolicy {
            certificate_ref: Some("runtime-cert.pem".to_string()),
            private_key_ref: Some("runtime-key.pem".to_string()),
            https_redirect: Some(true),
            canonical_host: Some("example.com".to_string()),
        });

        let mut state = RuntimeState::default();
        state.upsert_service(service, None).unwrap();

        let merged = merge_runtime_config(&AppConfig::default(), &state).unwrap();
        let canonical_site = merged
            .sites
            .iter()
            .find(|site| site.host == "example.com")
            .unwrap();
        assert_eq!(
            canonical_site.tls,
            Some(SiteTlsConfig {
                cert: "runtime-cert.pem".to_string(),
                key: "runtime-key.pem".to_string(),
            })
        );

        let canonical_route = canonical_site
            .routes
            .iter()
            .find(|route| route.path == "/api/*")
            .unwrap();
        assert!(matches!(canonical_route.handler, HandlerConfig::Proxy(_)));
        assert!(
            canonical_route
                .middlewares
                .iter()
                .any(|middleware| matches!(middleware, HoopConfig::ForceHttps { .. }))
        );

        let redirect_site = merged
            .sites
            .iter()
            .find(|site| site.host == "www.example.com")
            .unwrap();
        assert_eq!(redirect_site.tls, canonical_site.tls);

        let redirect_route = redirect_site
            .routes
            .iter()
            .find(|route| route.path == "/api/*")
            .unwrap();
        assert!(redirect_route.matchers.iter().any(
            |matcher| matches!(matcher, RequestMatcher::Protocol(protocol) if protocol == "https")
        ));
        match &redirect_route.handler {
            HandlerConfig::Redirect { to, permanent } => {
                assert_eq!(to, "https://example.com{path}{query_suffix}");
                assert!(*permanent);
            }
            other => panic!("unexpected handler: {other:?}"),
        }
    }
}
