# Future work

Automated release infrastructure is deferred. When it becomes useful, the next
release milestone can add:

- Windows CI for formatting, linting, tests, and a locked release build.
- A tag-triggered release workflow that uploads the executable ZIP and SHA-256
  checksum, and records build provenance.
- Code signing, after choosing a certificate or an open-source signing program.
- An optional WinGet manifest once releases are automated.

Releases currently use a manual, single-user flow: locally committed source is
built as a candidate, with its test suffix recorded in local build metadata or
the candidate-specific path while Cargo and the executable retain the stable
target version. The user closes the old app, reopens that exact candidate, and
tests it. No commit is pushed and no tag is created until the user approves
the candidate. After approval, the stable commit and annotated tag are pushed
to Gitea and checked on its one-way GitHub mirror. The approved local
executable is packaged as a ZIP with a SHA-256 checksum and attached to a
GitHub release without CI. Gitea publishes source and tags; GitHub hosts the
release assets. See the [release procedure](development.md#manual-single-user-release-flow)
and [release notes template](release-notes-template.md).
