<div align="center">

# Wheeped

A Minecraft server in Rust on the **Sonix** engine, built for Wureta Network.
Wheeped is a fork of [Pumpkin](https://github.com/Pumpkin-MC/Pumpkin).

[![License: GPL](https://img.shields.io/badge/License-GPLv3-yellow.svg)](LICENSE)

</div>

> [!IMPORTANT]
> Wheeped is under heavy development and is **not qualified for production**.
> Almost everything a player sees today is Pumpkin's work. What Wheeped adds on top is listed, with
> its real status, in [docs/sonix/ARCHITECTURE.md](docs/sonix/ARCHITECTURE.md).

## What Wheeped is

- **Wheeped** is the server. **Sonix** is the name of the engine work done in this fork: world
  storage, player profiles, the services a network of servers needs, and the API that first-party
  plugins are written against.
- It starts from Pumpkin, a Minecraft server written from scratch in Rust, and follows it:
  Wheeped's changes live on `main`, and Pumpkin's work keeps being merged into it.
- The systems that come from [SourbyCraft](https://github.com/YanIanZ/SourbyCraft) (Aurora World
  Fabric, MultiProfile and others) are being ported from Java to Rust, unit by unit, each held to the
  behaviour of the Java original. Progress is in [docs/sonix/PORT-MAP.md](docs/sonix/PORT-MAP.md).

Wheeped does not load Bukkit, Paper or Fabric plugins. Plugins are written in Rust.

## Build and run

Rust stable (see `rust-toolchain.toml`; the workspace needs at least the version in `Cargo.toml`).

```sh
cargo check --locked -p pumpkin        # compile check
cargo run --locked -p pumpkin          # debug server, writes config and worlds into the working directory
cargo run --locked --release -p pumpkin
```

The crates and the binary still carry Pumpkin's names (`pumpkin-*`); see
[docs/sonix/UPSTREAM.md](docs/sonix/UPSTREAM.md) for why.

Two defaults differ from Pumpkin on purpose: telemetry and the update checker are **off**. Both
talk to Pumpkin's services, and a fork should not report into upstream's statistics or advertise
upstream's releases as its own.

## Documentation

| Document | What it covers |
| :-- | :-- |
| [docs/sonix/ARCHITECTURE.md](docs/sonix/ARCHITECTURE.md) | What Sonix is, what exists today and what is only planned |
| [docs/sonix/DECISIONS.md](docs/sonix/DECISIONS.md) | Decision records: accepted, and proposed ones waiting for the owner |
| [docs/sonix/PORT-MAP.md](docs/sonix/PORT-MAP.md) | The Java to Rust port: units, order, state |
| [docs/sonix/GLOSSARY.md](docs/sonix/GLOSSARY.md) | Domain terms and their names in Rust |
| [docs/sonix/UPSTREAM.md](docs/sonix/UPSTREAM.md) | Following Pumpkin: branches, merges, the list of touched upstream files |
| [AGENTS.md](AGENTS.md) | Rules for coding agents: Wheeped's first, then Pumpkin's |

Pumpkin's own documentation at [docs.pumpkinmc.org](https://docs.pumpkinmc.org/) describes the
server Wheeped is built on.

## Issues and support

Report Wheeped problems in this repository's issue tracker. Please do not take them to Pumpkin's
tracker or Discord: its maintainers do not support forks, and a bug seen on Wheeped has to be
reproduced on unmodified Pumpkin before it is theirs.

## License and attribution

Wheeped is a modified version of Pumpkin, forked on 2026-11-23 from commit
[`a49be4bee`](https://github.com/Pumpkin-MC/Pumpkin/commit/a49be4bee2113e4cf812de3117c61bc3e9d06d3f).
Every change since then is in this repository's history (`git log upstream/master..main` in a clone
that has Pumpkin as its `upstream` remote).

- **Server** (Pumpkin's crates and Wheeped's `sonix-*` crates): [GNU General Public License v3.0](LICENSE).
- **Plugin API** (`pumpkin-plugin-api`, `pumpkin-plugin-wit`): dual-licensed
  [MIT](crates/pumpkin-plugin-api/LICENSE-MIT) OR [Apache-2.0](crates/pumpkin-plugin-api/LICENSE-APACHE), as upstream.
- **Third-party assets and data**: Bedrock mappings, protocol conversion data and Minecraft assets
  keep their own licenses and attribution terms, see [assets/NOTICE.md](assets/NOTICE.md).

Pumpkin is the work of its [contributors](https://github.com/Pumpkin-MC/Pumpkin/graphs/contributors).
If Wheeped is useful to you, consider [supporting Pumpkin](https://pumpkinmc.org/donate/).
