//! CLI artifacts come from the executable command tree, never a second schema.
use std::io::{self, Write};
use std::process::ExitCode;

pub(crate) fn completions(mut command: clap::Command, shell: clap_complete::Shell) -> ExitCode {
    // Generators may panic on a failed writer. Render to memory so an ordinary
    // closed output pipe is handled by our CLI policy, not a generator panic.
    let mut bytes = Vec::new();
    clap_complete::generate(shell, &mut command, "gatel", &mut bytes);
    publish(&bytes)
}

pub(crate) fn format(path: &std::path::Path, check: bool) -> ExitCode {
    match gatel_kdl::format_file(path) {
        Ok(formatted) if check => {
            if formatted.changed {
                eprintln!("gatel: {} needs formatting", path.display());
                ExitCode::from(super::CONFIG_REJECTED)
            } else {
                ExitCode::SUCCESS
            }
        }
        Ok(formatted) => publish(formatted.text.as_bytes()),
        Err(diagnostics) => {
            eprintln!("{diagnostics}");
            ExitCode::from(super::CONFIG_REJECTED)
        }
    }
}

pub(crate) fn man_page(mut command: clap::Command, path: &[String]) -> ExitCode {
    command.build();
    let mut selected = &command;
    for segment in path {
        let Some(child) = selected.find_subcommand(segment) else {
            eprintln!("gatel: unknown manual subcommand: {segment}");
            return ExitCode::from(2);
        };
        selected = child;
    }
    let mut bytes = Vec::new();
    if let Err(error) = clap_mangen::Man::new(selected.clone())
        .source(format!("gatel {}", env!("CARGO_PKG_VERSION")))
        .render(&mut bytes)
    {
        eprintln!("gatel: cannot render manual: {error}");
        return ExitCode::from(super::RUNTIME_FAILURE);
    }
    publish(&bytes)
}

fn publish(bytes: &[u8]) -> ExitCode {
    match write_artifact(&mut io::stdout().lock(), bytes) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("gatel: cannot write CLI artifact: {error}");
            ExitCode::from(super::RUNTIME_FAILURE)
        }
    }
}

fn write_artifact(writer: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    match writer.write_all(bytes).and_then(|()| writer.flush()) {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Failed(io::ErrorKind, bool);
    impl Write for Failed {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.1 {
                Ok(bytes.len())
            } else {
                Err(self.0.into())
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(self.0.into())
        }
    }
    #[test]
    fn pipe_closure_is_normal_but_other_write_and_flush_errors_are_not() {
        for flush in [false, true] {
            assert!(write_artifact(&mut Failed(io::ErrorKind::BrokenPipe, flush), b"manual").is_ok());
            assert!(write_artifact(&mut Failed(io::ErrorKind::PermissionDenied, flush), b"manual").is_err());
        }
    }
}
