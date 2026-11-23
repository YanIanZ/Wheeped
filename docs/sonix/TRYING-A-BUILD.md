# Trying a build

How to get a Linux binary of a branch and put it in front of a proxy on a test server. Nothing here
has been run by an agent: no workflow has run in this fork yet, and no Wheeped build has been
started on any server. Treat the first pass through it as a test of these instructions as well.

## 1. Turn on workflows in the fork (once)

GitHub keeps workflows off in a new fork. In the repository, open **Actions** and confirm that the
workflows may run. From then on:

- a pull request runs formatting, lints and the tests of the crates it touches;
- a push to `main` runs the full suite and builds release binaries;
- a push to `master`, which only mirrors Pumpkin, runs neither.

The spelling check runs on every push. The release, nightly, Docker, Nix and WIT mirror workflows
are Pumpkin's: their jobs are limited to Pumpkin's repository and are skipped here.

## 2. Build a binary from a branch

**Actions**, then **Cargo Build, Test, and Linting**, then **Run workflow**, and pick the branch. A
manual run is a full run: it ends with the job "Build project in release", which uploads one
artifact per platform. The one for the game nodes is `pumpkin-X64-Linux`.

Two things to know about that artifact:

- It is built without link-time optimisation and with 16 codegen units (the workflow's defaults for
  speed). For a performance comparison, start the run with **LTO mode** `fat` and **codegen units**
  `1`, which is what Pumpkin's shipped binaries use. It takes much longer.
- The binary is still called `pumpkin`. See [UPSTREAM.md](UPSTREAM.md) for why names are unchanged.

From a terminal: `gh workflow run rust.yml --repo YanIanZ/Wheeped --ref <branch> -f release_lto=fat -f release_codegen_units=1`,
then `gh run download --repo YanIanZ/Wheeped --name pumpkin-X64-Linux` once it has finished.

## 3. Start it

The server writes its configuration and worlds into the directory it is started from, so use an
empty one. The first start writes `pumpkin.toml` with every key and its default.

```sh
chmod +x pumpkin-X64-Linux && ./pumpkin-X64-Linux
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
