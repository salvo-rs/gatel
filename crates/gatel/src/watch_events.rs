//! Filter ordinary writes without filesystem I/O on the notification thread.
use std::collections::BTreeSet;
use std::ffi::OsString;

use notify::event::ModifyKind;
use notify::{Event, EventKind};

pub(super) fn relevant(event: &Event, names: Option<&BTreeSet<OsString>>) -> bool {
    if event.need_rescan() {
        return true;
    }
    match event.kind {
        // Unknown/new names may change import glob expansion or replace files.
        EventKind::Any
        | EventKind::Create(_)
        | EventKind::Remove(_)
        | EventKind::Modify(ModifyKind::Name(_)) => true,
        EventKind::Modify(_) => names.is_none_or(|names| {
            event.paths.is_empty()
                || event
                    .paths
                    .iter()
                    .any(|path| path.file_name().is_some_and(|name| names.contains(name)))
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use notify::event::{CreateKind, DataChange, RemoveKind, RenameMode};

    use super::*;
    #[test]
    fn log_writes_are_ignored_but_configuration_and_topology_changes_are_not() {
        let names = BTreeSet::from([OsString::from("routes.conf")]);
        let write = |path: &str| {
            Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any))).add_path(path.into())
        };
        assert!(!relevant(&write("/config/events.log"), Some(&names)));
        assert!(relevant(&write("/config/routes.conf"), Some(&names)));
        assert!(relevant(&write("/other/routes.conf"), Some(&names))); // conservative basename match
        assert!(relevant(&write("/config/events.log"), None)); // discovery not ready
        for kind in [
            EventKind::Create(CreateKind::File),
            EventKind::Remove(RemoveKind::File),
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
        ] {
            assert!(relevant(
                &Event::new(kind).add_path("/config/new.conf".into()),
                Some(&names)
            ));
        }
        assert!(relevant(
            &Event::new(EventKind::Modify(ModifyKind::Any)),
            Some(&names)
        ));
        assert!(!relevant(
            &Event::new(EventKind::Access(notify::event::AccessKind::Any)),
            Some(&names)
        ));
    }
}
