//! Request-scoped state container.
//!
//! [`Depot`] allows middleware to store typed values that downstream
//! middleware and handlers can retrieve.

use std::any::Any;
use std::collections::HashMap;

/// A request-scoped key-value store for passing data between middleware.
#[derive(Default)]
pub struct Depot {
    map: HashMap<String, Box<dyn Any + Send + Sync>>,
}

impl Depot {
    #[inline]
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    #[inline]
    pub fn inject<V: Any + Send + Sync>(&mut self, value: V) -> &mut Self {
        self.map.insert(type_key::<V>(), Box::new(value));
        self
    }

    #[inline]
    pub fn obtain<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.map
            .get(&type_key::<T>())
            .and_then(|value| value.downcast_ref::<T>())
    }

    #[inline]
    pub fn scrape<T: Any + Send + Sync>(&mut self) -> Option<T> {
        self.map
            .remove(&type_key::<T>())
            .and_then(|value| value.downcast::<T>().ok().map(|value| *value))
    }

    #[inline]
    pub fn insert<K: Into<String>, V: Any + Send + Sync>(&mut self, key: K, value: V) -> &mut Self {
        self.map.insert(key.into(), Box::new(value));
        self
    }

    #[inline]
    pub fn get<V: Any + Send + Sync>(&self, key: &str) -> Option<&V> {
        self.map
            .get(key)
            .and_then(|value| value.downcast_ref::<V>())
    }

    #[inline]
    pub fn get_mut<V: Any + Send + Sync>(&mut self, key: &str) -> Option<&mut V> {
        self.map
            .get_mut(key)
            .and_then(|value| value.downcast_mut::<V>())
    }

    #[inline]
    pub fn remove<V: Any + Send + Sync>(&mut self, key: &str) -> Option<V> {
        self.map
            .remove(key)
            .and_then(|value| value.downcast::<V>().ok().map(|value| *value))
    }

    #[inline]
    pub fn contains_key(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl std::fmt::Debug for Depot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Depot")
            .field("keys", &self.map.keys().collect::<Vec<_>>())
            .finish()
    }
}

fn type_key<T: 'static>() -> String {
    format!("__type_{:?}", std::any::TypeId::of::<T>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_get_by_string_key() {
        let mut depot = Depot::new();
        depot.insert("name", "alice".to_string());
        assert_eq!(depot.get::<String>("name"), Some(&"alice".to_string()));
        assert!(depot.get::<u32>("name").is_none());
        assert!(depot.get::<String>("other").is_none());
    }

    #[test]
    fn inject_and_obtain_by_type() {
        let mut depot = Depot::new();
        depot.inject(42u64);
        assert_eq!(depot.obtain::<u64>(), Some(&42));
        assert!(depot.obtain::<u32>().is_none());
    }

    #[test]
    fn remove_returns_value() {
        let mut depot = Depot::new();
        depot.insert("x", 100u32);
        assert_eq!(depot.remove::<u32>("x"), Some(100));
        assert!(depot.get::<u32>("x").is_none());
    }

    #[test]
    fn scrape_removes_typed_value() {
        let mut depot = Depot::new();
        depot.inject(String::from("hello"));
        assert_eq!(depot.scrape::<String>(), Some(String::from("hello")));
        assert!(depot.obtain::<String>().is_none());
    }
}
