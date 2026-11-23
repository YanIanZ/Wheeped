# Trying a build

How to get a Linux binary of a branch and put it in front of a proxy on a test server. Pull request
checks have been running in this fork since 2026-11-23. The build workflow of step 2 has not run
yet, and no Wheeped build has been started on any server, so everything from step 2 on is untested:
treat the first pass through it as a test of these instructions as well.

## 1. Turn on workflows in the fork (once)

GitHub keeps workflows off in a new fork until they are confirmed in the repository's **Actions**
tab. That was done on 2026-11-23. Since then:

- a pull request runs formatting, lints and the tests of the crates it touches;
- a push to `main` runs the full suite.

`master` is never pushed: see [UPSTREAM.md](UPSTREAM.md) for why.

The spelling check runs on every push. The release, nightly, Docker, Nix and WIT mirror workflows
are Pumpkin's: their jobs are limited to Pumpkin's repository and are skipped here.

## 2. Build a binary from a branch

**Actions**, then **Wheeped build**, then **Run workflow**, and pick the branch. The workflow is on
`main`, so it is there once this file is. It builds the release profile as it is (full link-time
optimisation, one codegen unit, which is what Pumpkin's shipped binaries use) and uploads two
artifacts:

- `wheeped-X64-Linux`: runs on any 64-bit x86 CPU.
- `wheeped-X64-v3-Linux`: for CPUs with AVX2 and BMI2 (Intel Haswell and newer, AMD Zen and newer),
  usually the faster of the two. A CPU without them stops it with an illegal instruction.

It publishes nothing: the binaries are artifacts of the run, kept for GitHub's retention period.
Inside the artifact the file carries the artifact's name; in the source tree the binary is still
called `pumpkin` (see [UPSTREAM.md](UPSTREAM.md) for why names are unchanged).

From a terminal: `gh workflow run wheeped-build.yml --repo YanIanZ/Wheeped --ref <branch>`, then
`gh run download --repo YanIanZ/Wheeped --name wheeped-X64-v3-Linux` once it has finished. A full
build of this kind takes long, often more than half an hour.

The big CI workflow also has a release build at its end, but only for pushes and manual runs, and
only when every test job of that run passed ([UPSTREAM.md](UPSTREAM.md) has the state of those jobs).

## 3. Start it

The server writes its configuration and worlds into the directory it is started from, so use an
empty one. The first start writes `pumpkin.toml` with every key and its default.

```sh
chmod +x wheeped-X64-v3-Linux && ./wheeped-X64-v3-Linux
```

Type `stop` in its console to shut it down.

## 4. Put it behind the proxy

The settings live in `pumpkin.toml`. The names below are the configuration's own (the structs in
`crates/pumpkin-config/src/networking/`); find them in the generated file rather than typing a
section by hand.

| Setting | Value for a backend behind a Velocity-compatible proxy |
| :-- | :-- |
| `networking.java.address` | the address and port the proxy connects to |
| `networking.java.online_mode` | `false`: the proxy authenticates players |
| `networking.proxy.enabled` | `true` |
| `networking.proxy.velocity.enabled` | `true` |
| `networking.proxy.velocity.secret` | the proxy's forwarding secret |

The secret is a credential: it goes into the file on the server, never into this repository, an
issue or a log that leaves the machine. A backend must be reachable from the proxy only.

Wheeped speaks one Java protocol version, the one named in the startup line. A client on another
version is refused unless the proxy, or a plugin that registers more versions, translates for it.

## 5. What to look at

- The startup line names Wheeped, the Pumpkin version it is based on and the protocol.
- `/pumpkin` prints build information. `/tps` and `/tick query` report tick times.
- For a comparison with another server: the same world, the same player count and view distance, the
  same duration on both, and more than one run each. One favourable run is not a result.

## On a panel

`egg-pumpkin.json` in the repository root is Pumpkin's egg for Pterodactyl and Pelican. It installs a
binary from a GitHub repository's releases and takes the repository from its `GITHUB_PACKAGE`
variable, or a direct address from `DOWNLOAD_URL`. Wheeped publishes no releases yet, so on a panel
the binary from step 2 is uploaded by hand for now.
