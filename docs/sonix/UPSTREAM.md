# Following Pumpkin

Wheeped is only useful while it can keep taking Pumpkin's work. This page is how that is kept
possible.

## Branches and remotes

| Name | What it is |
| :-- | :-- |
| `upstream` | `https://github.com/Pumpkin-MC/Pumpkin.git`, fetch only. The local push URL is disabled on purpose. |
| `origin` | `https://github.com/YanIanZ/Wheeped.git` |
| `master` | Mirror of `upstream/master`. Fast-forward only, no Wheeped commits. |
| `main` | Wheeped. Default branch. Changes arrive through pull requests. |

## Taking upstream changes

```sh
git fetch upstream
git checkout master && git merge --ff-only upstream/master && git push origin master
git checkout -b sync/upstream-YYYYMMDD main
git merge master            # resolve conflicts here, on the branch
cargo check --locked -p pumpkin
git push -u origin sync/upstream-YYYYMMDD
gh pr create --repo YanIanZ/Wheeped --base main --title "chore(upstream): merge Pumpkin <short sha>"
```

A merge, not a rebase: `main` keeps Pumpkin's history, so `git log master..main` is always exactly
what Wheeped changed.

After a merge, check two things that fail quietly: the native plugin `PLUGIN_API_VERSION` (a bump
means every native plugin is rebuilt), and the list below (an upstream rewrite of a touched file can
drop a Wheeped edit without a conflict).

## Rules that keep merges cheap

1. New code goes in new crates, `crates/sonix-*`. A new crate never conflicts.
2. A Pumpkin file is edited only to call into Sonix or to change a default, and the edit is as small
   as it can be: a line, a call, a registration.
3. Crate, package and binary names stay `pumpkin-*`. The product name lives in strings and documents.
4. Formatting and refactoring of Pumpkin's files is never part of a Wheeped change.
5. Every touched Pumpkin file is in the list below, with the reason, added in the same change.

## Pumpkin files Wheeped has changed

| File | Change | Why |
| :-- | :-- | :-- |
| `README.md` | Replaced | The fork's own front page; states the base commit and the licence |
| `AGENTS.md` | A Wheeped section inserted at the top; Pumpkin's text unchanged | Agent rules for this repository |
| `.github/CODEOWNERS` | Replaced | Pumpkin's owners do not review this fork |
| `.github/PULL_REQUEST_TEMPLATE.md` | Replaced | The template pointed at Pumpkin's contribution rules |
| `crates/pumpkin-core/src/server/connection_cache.rs` | Brand string `Pumpkin` to `Wheeped` | What a client shows as the server brand |
| `crates/pumpkin/src/main.rs` | Startup banner and support links | Names the fork and the base; sends issue reports to this repository, not to Pumpkin |
| `crates/pumpkin-config/src/telemetry.rs` | Default `enabled` true to false | The endpoint is Pumpkin's; a fork does not report into it unasked |
| `crates/pumpkin-config/src/update_checker.rs` | Default `enabled` true to false | It checks Pumpkin's releases, which are not Wheeped builds |
| `Cargo.toml`, `Cargo.lock` | Workspace member `crates/sonix-awf` and its lock entry | A new crate has to be listed |
| `typos.toml` | `crates/sonix-awf/tests/fixtures/java-b57` excluded | Hash-linked binary fixtures full of hex names |

## Workflows

The fork carries Pumpkin's GitHub workflows. GitHub keeps workflows disabled on a new fork until the
owner enables them. `docker.yml` and `nightly.yml` already run only in `Pumpkin-MC/Pumpkin`.
`release.yml` and `sync-wit.yml` need secrets this fork does not have and would fail if triggered;
they are untouched for now and should be limited to the upstream repository before workflows are
enabled here.

## Sending a change to Pumpkin

A fix that belongs upstream goes to Pumpkin as its own branch from `master`, under Pumpkin's rules
in `AGENTS.md` and `CONTRIBUTING.md`: the owner opens that pull request himself.
