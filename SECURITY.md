# Security policy

## Reporting a vulnerability

Please **do not** open a public issue for security problems.

Report privately instead, either way works:

- GitHub: **Security → Report a vulnerability** on this repository (private advisory).
- LinkedIn: message [Vitalii Ermolaev](https://www.linkedin.com/in/vitalii-ermolaev/).

Include what you found, how to reproduce it, and what an attacker gains. This is a
solo-maintained project, so there is no bug bounty — but credit in the fix is yours
if you want it.

## Scope

In scope: the server in this repository — authentication and sessions, the
WebSocket handshake and protocol handling, rate limiting and connection caps, and
anything that lets a client forge a gameplay outcome (eat/death claims, scoring,
rewards) the server should have rejected.

Out of scope: deployments you run yourself with dev settings (`FAIRTICK_DEV=1`,
`FAIRTICK_AUTH_MODE=insecure`, test tokens) — those are insecure by design, and the
server refuses to boot with them unless `FAIRTICK_DEV=1` is set explicitly.

The last full audit and its remediation status are in
[`docs/security-audit-2026-06-11.md`](docs/security-audit-2026-06-11.md).
