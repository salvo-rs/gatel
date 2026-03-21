# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in Gatel, please report it responsibly.

**Do NOT open a public GitHub issue for security vulnerabilities.**

Instead, please report security issues by emailing the maintainers directly or
using [GitHub's private vulnerability reporting](https://github.com/salvo-rs/gatel/security/advisories/new).

Include the following in your report:

- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if any)

We will acknowledge receipt within 48 hours and aim to provide a fix within 7
days for critical issues.

## Supported Versions

| Version | Supported          |
|---------|--------------------|
| latest  | Yes                |
| < latest| Best effort        |

## Security Best Practices

When deploying Gatel in production:

- **Keep Gatel updated** to the latest version.
- **Use TLS** for all public-facing traffic (`tls` block in config).
- **Restrict the admin API** — bind it to `127.0.0.1` or use firewall rules.
- **Limit file server roots** — avoid serving sensitive directories.
- **Use rate limiting** to protect against abuse.
- **Run as a non-root user** — the systemd unit drops privileges automatically.
- **Review config** with `gatel validate` before deploying changes.

## Disclosure Policy

- We follow [coordinated disclosure](https://en.wikipedia.org/wiki/Coordinated_vulnerability_disclosure).
- Security fixes are released as patch versions.
- A security advisory is published on GitHub after the fix is available.
