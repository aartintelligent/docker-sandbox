## Summary

<!-- What does this change and why? Link the issue it closes, e.g. "Closes #12". -->

## Checklist

- [ ] `just ci` passes locally (fmt, clippy, typos, tests, doctests, cargo-deny, privacy)
- [ ] Behaviour changes come with tests; a new daemon route or field comes with an audit scenario and a fixture
- [ ] `just test-live` passes when the change touches the library's daemon calls
- [ ] Public API changes are documented (rustdoc, README when user-facing) and called out above
- [ ] Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/) (see `CONTRIBUTING.md`)
- [ ] Breaking changes are marked with `!` in the commit type (e.g. `feat(sandbox)!:`)
