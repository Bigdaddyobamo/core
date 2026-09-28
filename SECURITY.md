# Security Policy

## Supported versions

| Version        | Supported          |
|----------------|--------------------|
| `main`         | :white_check_mark: |
| latest release | :white_check_mark: |
| older releases | :x:                |

## Reporting a vulnerability

**Please do not report security vulnerabilities through public GitHub issues,
discussions or pull requests.**

Report them privately through GitHub's private vulnerability reporting:
go to the repository's **Security** tab and choose
**Report a vulnerability**
(<https://github.com/Tx-wats/core/security/advisories/new>).

Please include:

- a description of the issue and its impact
- the affected version or commit
- steps to reproduce, or a proof of concept
- any suggested fix

We aim to acknowledge reports within 3 business days and to share a fix or
mitigation plan within 14 days. We will credit reporters in the advisory
unless you ask us not to.

## Scope

TxWatch handles webhook secrets and forwards alert data to external
endpoints. Issues such as secret leakage in requests or logs, signature
bypasses, and request forgery against configured webhooks are in scope.
For a detailed analysis of trust boundaries, assets, and attack surfaces, see [Threat Model](docs/THREAT_MODEL.md).
