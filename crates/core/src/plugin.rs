//! Dynamic module/plugin system for gatel.
//!
//! External crates can implement the [`ModuleLoader`] trait to register
//! custom middleware and handlers. Modules are loaded during configuration
//! parsing and can participate in the request/response lifecycle.

use std::collections::HashMap;
use std::sync::Arc;

use crate::ProxyError;

/// Result type for module operations.
pub type ModuleResult<T> = Result<T, ProxyError>;

/// A module loader creates module instances from configuration.
///
/// Implement this trait to add custom functionality to gatel. Each loader
/// is responsible for a named directive (e.g., "my-custom-middleware") and
/// creates `Module` instances when that directive appears in the config.
pub trait ModuleLoader: Send + Sync {
    /// The directive name that triggers this module (e.g., "waf", "graphql").
    fn name(&self) -> &str;

    /// Validate the configuration for this module.
    /// Called during config parsing before the server starts.
    /// Return Ok(()) if the config is valid, or an error describing the issue.
    fn validate_config(&self, config: &HashMap<String, String>) -> ModuleResult<()> {
        let _ = config;
        Ok(())
    }

    /// Create a middleware instance from the given configuration.
    /// Return None if this module does not provide middleware.
    fn create_middleware(
        &self,
        config: &HashMap<String, String>,
    ) -> ModuleResult<Option<Arc<dyn salvo::Handler>>> {
        let _ = config;
        Ok(None)
    }

    /// Create a handler instance from the given configuration.
    /// Return None if this module does not provide a terminal handler.
    fn create_handler(
        &self,
        config: &HashMap<String, String>,
    ) -> ModuleResult<Option<Arc<dyn salvo::Handler>>> {
        let _ = config;
        Ok(None)
    }

    /// Called once when the module is loaded (server startup).
    fn on_load(&self) -> ModuleResult<()> {
        Ok(())
    }

    /// Called when the server is shutting down.
    fn on_unload(&self) -> ModuleResult<()> {
        Ok(())
    }

    /// Called when configuration is reloaded (hot-reload).
    fn on_reload(&self, config: &HashMap<String, String>) -> ModuleResult<()> {
        let _ = config;
        Ok(())
    }
}

/// Registry of available module loaders.
///
/// Modules are registered at startup and looked up by name during
/// configuration parsing. The registry is thread-safe and can be
/// shared across the application.
pub struct ModuleRegistry {
    loaders: HashMap<String, Box<dyn ModuleLoader>>,
}

impl ModuleRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            loaders: HashMap::new(),
        }
    }

    /// Register a module loader. If a loader with the same name already
    /// exists, it is replaced.
    pub fn register(&mut self, loader: Box<dyn ModuleLoader>) {
        let name = loader.name().to_string();
        tracing::info!(module = %name, "registered module loader");
        self.loaders.insert(name, loader);
    }

    /// Look up a loader by directive name.
    pub fn get(&self, name: &str) -> Option<&dyn ModuleLoader> {
        self.loaders.get(name).map(|b| b.as_ref())
    }

    /// Iterate over all registered loaders.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &dyn ModuleLoader)> {
        self.loaders.iter().map(|(k, v)| (k.as_str(), v.as_ref()))
    }

    /// Call `on_load` on all registered modules.
    pub fn load_all(&self) -> ModuleResult<()> {
        for (name, loader) in &self.loaders {
            loader.on_load().map_err(|e| {
                ProxyError::Internal(format!("module '{name}' on_load failed: {e}"))
            })?;
        }
        Ok(())
    }

    /// Call `on_unload` on all registered modules.
    pub fn unload_all(&self) {
        for (name, loader) in &self.loaders {
            if let Err(e) = loader.on_unload() {
                tracing::warn!(module = %name, error = %e, "module on_unload failed");
            }
        }
    }

    /// Call `on_reload` on all registered modules.
    pub fn reload_all(&self, configs: &HashMap<String, HashMap<String, String>>) {
        for (name, loader) in &self.loaders {
            if let Some(cfg) = configs.get(name.as_str()) {
                if let Err(e) = loader.on_reload(cfg) {
                    tracing::warn!(module = %name, error = %e, "module on_reload failed");
                }
            }
        }
    }

    /// Number of registered modules.
    pub fn len(&self) -> usize {
        self.loaders.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.loaders.is_empty()
    }
}

impl Default for ModuleRegistry {
    fn default() -> Self {
        Self::new()
    }
}
