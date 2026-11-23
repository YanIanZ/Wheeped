# Following Pumpkin

Wheeped is only useful while it can keep taking Pumpkin's work. This page is how that is kept
possible.

## Branches and remotes

| Name | What it is |
| :-- | :-- |
| `upstream` | `https://github.com/Pumpkin-MC/Pumpkin.git`, fetch only. The local push URL is disabled on purpose. |
| `origin` | `https://github.com/YanIanZ/Wheeped.git` |
| `master` | Pumpkin's branch as it was at the fork point. No Wheeped commits, and never pushed: see below. The live mirror is `upstream/master` in a clone. |
| `main` | Wheeped. Default branch. Changes arrive through pull requests. |

## Taking upstream changes

```sh
git fetch upstream
git checkout -b sync/upstream-YYYYMMDD main
git merge upstream/master   # resolve conflicts here, on the branch
cargo check --locked -p pumpkin
git push -u origin sync/upstream-YYYYMMDD
gh pr create --repo YanIanZ/Wheeped --base main --title "chore(upstream): merge Pumpkin <short sha>"
```

A merge, not a rebase: `main` keeps Pumpkin's history, so after a fetch `git log upstream/master..main`
is always exactly what Wheeped changed.

**Do not push `master` to this repository, and do not use GitHub's "Sync fork" on it.** A push runs
the workflow files of the branch that was pushed, and `master` carries Pumpkin's untouched ones:
the full CI and, when it passes, Pumpkin's canary job, which force-moves a `canary` tag and
publishes a prerelease with binaries in this repository. So `master` on GitHub stays where the fork
was made, and upstream is merged from the `upstream` remote of a clone.

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
| `.github/workflows/rust.yml` | Push trigger `master` to `main` | `main` is the fork's branch. The canary release job still asks for `master`, so it never runs from `main` |
| `.github/workflows/release.yml`, `nix.yml`, `sync-wit.yml` | Jobs limited to `Pumpkin-MC/Pumpkin` | They need Pumpkin's secrets or serve Pumpkin's packaging |
| `crates/pumpkin-core/src/block/blocks/redstone/daylight_detector.rs` | `update_signal_strength` returns when the block is no longer a daylight detector | Its block entity can be ticked after the block was replaced, and the update then panicked the simulation thread |
| `tools/pumpkin-tester/src/bot_tests/snow_golem.rs` | The snow formation check cancels events before it registers its handler, and counts at least four events per step instead of exactly four | The server ticks the test's golem at the same time, so the exact counts failed whenever a tick landed inside the check |
| `crates/pumpkin-plugin-runtime/src/lib.rs` (its tests only) | Budgets for a call to complete raised from 2 s and 10 s to 30 s and 120 s | The Android emulator of the CI can need more than 2 s for one of these calls |

## Workflows

The fork carries Pumpkin's GitHub workflows, enabled here on 2026-11-23. `rust.yml` (formatting, lints, tests, builds) runs for pull requests and for
pushes to `main`; `typos.yml` runs on every push. `docker.yml` and `nightly.yml` came limited to
`Pumpkin-MC/Pumpkin`, and `release.yml`, `nix.yml` and `sync-wit.yml` are limited to it here: their
jobs are skipped in this repository.

`wheeped-build.yml` is this fork's own: started by hand, it builds a Linux binary of any branch
without waiting for the test matrix and publishes nothing. [TRYING-A-BUILD.md](TRYING-A-BUILD.md)
says how to use it.

Three of Pumpkin's checks failed here on 2026-11-23, at the base commit `a49be4bee` and on changes
that could not have caused it. Each is changed in this fork; the files are in the list above.

- "GameTest and Bot Harness" panicked the simulation thread twice (`air is not a valid block for
  DaylightDetectorLikeProperties`). A daylight detector's block entity was ticked after its block
  had been replaced, and the update did not check what block was there. It returns early now. The
  same panic would stop the simulation thread of a running server.
- The same job failed once with "Cancelled snow formation must leave all four positions unchanged",
  which is also how Pumpkin's own run for that commit failed. The bot test counted the formation
  events of a golem that the server ticks at the same time, so a tick inside the check changed the
  count. The test now cancels events before it listens and takes its counts as lower bounds.
- "Test on Android AArch64" failed once in `pumpkin-plugin-runtime` (`blocking sync-lifted guest
  reentry timed out`): a call had two seconds to complete, on an emulator. The budgets of those
  tests are longer now.

The three changes follow from reading the code. None of the failures was reproduced on purpose, and
a few green runs do not show that an intermittent failure is gone: if one of these returns, run the
job again and look at its log before reading it as a Wheeped failure. In the six runs the GameTest
job had completed here when the changes were written, it passed three times and failed three times.
Pumpkin's run for the base commit also lost "Test (Linux ARM64)" to a killed runner (exit code 143),
which no change here addresses.

The release build at the end of `rust.yml` runs only when every test job of the run passed, which is
the reason for the separate build workflow.

## Sending a change to Pumpkin

A fix that belongs upstream goes to Pumpkin as its own branch from `upstream/master`, under Pumpkin's rules
in `AGENTS.md` and `CONTRIBUTING.md`: the owner opens that pull request himself.
