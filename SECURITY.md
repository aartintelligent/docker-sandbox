# Security policy

## Supported versions

Only the latest version of each crate receives security fixes.

## Reporting a vulnerability

Please do **not** open a public issue for security problems.

Use GitHub's private vulnerability reporting on this repository
(*Security* tab → *Report a vulnerability*). You will get an acknowledgement
within 72 hours and a fix or mitigation plan within 14 days for confirmed
issues.

Vulnerabilities in Docker Sandboxes (`sbx`, `sandboxd`) themselves belong to
Docker, Inc.: report them through Docker's own vulnerability disclosure
process, not here.

## Scope notes

- The `sandboxd` socket has no authentication: whoever can open it controls
  every sandbox of the user. The library adds none; run the programs that use
  it as that user, and never expose the socket to other accounts.
- Secrets travel in clear text over the socket to the daemon, which keeps them
  out of the sandboxes behind placeholders; the library never logs them
  (`Secret` is redacted in `Debug`).
- `audit` scrubs personal data from everything it records; report any
  personal data found in a snapshot or a fixture as a security issue.
