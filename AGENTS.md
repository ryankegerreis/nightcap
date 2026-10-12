# Nightcap agent notes

Architecture and module ownership: [`docs/architecture.md`](docs/architecture.md).

## Pull request titles set the version

Pull requests are squash-merged, and the PR title becomes the commit on
`master`. release-please reads those titles to pick the next version and write
`CHANGELOG.md`, so the title is your release decision. Branch commit messages
are discarded at merge.

Title every PR `type: summary` (Conventional Commits). Choose the type by what
a Nightcap user notices after updating, not by the size of the diff:

| Title | Version bump | Use when the shipped app… |
|---|---|---|
| `feat!:` | 0.1.1 → 0.2.0 (major after 1.0) | breaks something users rely on: saved settings stop loading, a platform, end action or control is removed, or existing behaviour changes in a way users must adapt to |
| `feat:` | 0.1.1 → 0.2.0 (minor after 1.0) | gains something users can do: a feature, setting, tray action or platform |
| `fix:` | 0.1.1 → 0.1.2 | does something it always should have: a bug, crash, wrong restore or fade, platform quirk |
| `perf:` | 0.1.1 → 0.1.2 | does the same thing faster or with fewer resources |
| `revert:` | 0.1.1 → 0.1.2 | undoes an earlier released change |
| `docs:` `refactor:` `test:` `build:` `ci:` `chore:` | no release | is unchanged for users |

- A PR that mixes types takes the highest one in the table.
- A refactor or dependency update that changes behaviour is a `fix:` or
  `feat:`; title it by that behaviour.
- Write the summary as a changelog line for users, imperative and lowercase:
  `fix: restore volume on the original output device after wake`.
- The `PR title` check fails until the title matches this format.

release-please owns the `version` in `Cargo.toml`, `CHANGELOG.md`, tags and the
release PR (`chore(master): release x.y.z`). Leave all four to it; a release
ships when the maintainer merges that release PR.

To correct a PR that merged with the wrong title, edit the merged PR's
description and add the replacement message; release-please uses it on its
next run:

```
BEGIN_COMMIT_OVERRIDE
fix: the corrected summary
END_COMMIT_OVERRIDE
```
