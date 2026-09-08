//! Bounded, non-interactive password generation. Never format input in diagnostics.
use std::io::{self, IsTerminal, Read, Write};
use std::process::ExitCode;

use zeroize::Zeroizing;

// bcrypt 0.17's non-truncating interface reserves one byte for its terminator.
#[cfg(test)]
const MAX_PASSWORD: usize = 71;

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Algorithm {
    Bcrypt,
    #[cfg(feature = "argon2")]
    Argon2id,
}
impl Algorithm {
    fn policy(self) -> gatel_password::Algorithm {
        match self {
            Self::Bcrypt => gatel_password::Algorithm::Bcrypt,
            #[cfg(feature = "argon2")]
            Self::Argon2id => gatel_password::Algorithm::Argon2id,
        }
    }
}

pub(crate) fn run(algorithm: Algorithm, cost: Option<u32>) -> ExitCode {
    if algorithm != Algorithm::Bcrypt && cost.is_some() {
        eprintln!("gatel: --cost applies only to bcrypt");
        return ExitCode::from(2);
    }
    if io::stdin().is_terminal() {
        eprintln!("gatel: passwd requires piped or redirected stdin; terminal input is not hidden");
        return ExitCode::from(super::RUNTIME_FAILURE);
    }
    let result = generate_with(io::stdin().lock(), algorithm, cost).and_then(|hash| {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{}", hash.as_str())
            .and_then(|()| stdout.flush())
            .map_err(|_| "cannot write password hash")
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("gatel: {message}");
            ExitCode::from(super::RUNTIME_FAILURE)
        }
    }
}

fn generate_with(
    reader: impl Read,
    algorithm: Algorithm,
    cost: Option<u32>,
) -> Result<Zeroizing<String>, &'static str> {
    if cost.is_some_and(|cost| !(4..=16).contains(&cost)) {
        return Err("bcrypt cost must be 4..16");
    }
    // One optional CRLF plus one overflow sentinel. Never buffer unbounded input.
    let max_password = algorithm.policy().generation_limit();
    let mut input = Zeroizing::new(Vec::with_capacity(max_password + 3));
    reader
        .take((max_password + 3) as u64)
        .read_to_end(&mut input)
        .map_err(|_| "cannot read password input")?;
    if input.last() == Some(&b'\n') {
        input.pop();
        if input.last() == Some(&b'\r') {
            input.pop();
        }
    }
    if input.is_empty() || input.len() > max_password {
        return Err("password is empty or exceeds algorithm byte limit");
    }
    if input.iter().any(|byte| matches!(byte, 0 | b'\r' | b'\n')) {
        return Err("password must be one line without NUL bytes");
    }
    let password = std::str::from_utf8(&input).map_err(|_| "password must be UTF-8")?;
    gatel_password::generate(algorithm.policy(), password.as_bytes(), cost)
}

#[cfg(test)]
fn generate(reader: impl Read, cost: u32) -> Result<Zeroizing<String>, &'static str> {
    generate_with(reader, Algorithm::Bcrypt, Some(cost))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_spaces_and_handles_only_one_terminal_line_ending() {
        for input in [b" pass ".as_slice(), b" pass \n", b" pass \r\n"] {
            let hash = generate(input, 4).unwrap();
            assert!(bcrypt::verify(" pass ", &hash).unwrap());
            assert!(!bcrypt::verify("pass", &hash).unwrap());
        }
        for input in [b"".as_slice(), b"\n", b"x\n\n", b"x\r", b"x\0", b"\xff"] {
            assert!(generate(input, 4).is_err());
        }
    }
    #[test]
    fn byte_bounds_not_character_bounds_and_cost_are_enforced() {
        let input = [b'x'; MAX_PASSWORD];
        assert!(generate(input.as_slice(), 4).is_ok());
        assert!(generate([b'x'; MAX_PASSWORD + 1].as_slice(), 4).is_err());
        assert!(generate("界".repeat(24).as_bytes(), 4).is_err());
        assert!(generate(b"x".as_slice(), 3).is_err());
        assert!(generate(b"x".as_slice(), 17).is_err());
    }
    #[test]
    fn excessive_input_is_bounded_and_read_errors_are_redacted() {
        let mut infinite = io::repeat(b'x');
        assert!(generate(&mut infinite, 4).is_err());
        struct Failed;
        impl Read for Failed {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("private material"))
            }
        }
        assert_eq!(generate(Failed, 4).unwrap_err(), "cannot read password input");
    }
}
