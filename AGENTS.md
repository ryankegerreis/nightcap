# Nightcap agent notes

Architecture and module ownership: [`docs/architecture.md`](docs/architecture.md).

## Required cross-model code review

Before declaring any implementation task complete or handing it back to the
user, obtain an independent review of the final changes, including documentation
and configuration changes, from a different agent in the other model family:

- GPT / Codex implements → Claude reviews.
- Claude implements → GPT / Codex reviews.

Use an available model from the required family; a different model in the same
family or a self-review does not satisfy this requirement. In T3 Code, discover
providers and models with `orchestrator_capabilities` and use `delegate_task`
with an explicit provider and model for cross-provider review. Review the current
changes, including uncommitted edits; a separate worktree will not include those
edits unless they are explicitly supplied to the reviewer.

Give the reviewer the user's requirements, the exact diff or commit range, and
relevant validation results. The reviewer must inspect correctness, regressions,
missing requirements, and test coverage, and return actionable findings with
file references and severity (critical, important, or minor), or explicitly state
that no issues were found. Reviewers must remain read-only and must not delegate
further reviews. The review requirement applies only to the implementing agent;
review tasks do not trigger further reviews.

Resolve critical and important findings, run the relevant checks, and obtain
another review of those fixes before declaring completion. Explain any finding
you decline to change. In the final response, identify the reviewer model,
summarize the review outcome and validation, and disclose unresolved findings.
If the required reviewer is unavailable or fails, report that review is blocked
and the work is awaiting review; do not silently substitute a same-family agent
or claim the task is fully complete.

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
