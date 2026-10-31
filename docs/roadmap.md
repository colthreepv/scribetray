# Future work

Possible release improvements:

- Windows CI for formatting, linting, tests and locked release builds.
- Automatic release uploads with checksums and build records.
- Code signing and a WinGet package.

For now, releases use the tested local executable. After user approval, source
and tags go to Gitea and its GitHub mirror; downloads go to GitHub Releases.
See the [release procedure](development.md#release-procedure).
