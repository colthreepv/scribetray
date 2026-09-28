# Future work

Automated release infrastructure is deferred. When it becomes useful, the next
release milestone can add:

- Windows CI for formatting, linting, tests, and a locked release build.
- A tag-triggered release workflow that uploads the executable ZIP and SHA-256
  checksum, and records build provenance.
- Code signing, after choosing a certificate or an open-source signing program.
- An optional WinGet manifest once releases are automated.

The current v0.6.x release is built and published manually.
