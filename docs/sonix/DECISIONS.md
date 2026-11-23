# Decisions

Short decision records, numbered in order and never renumbered. A changed decision gets a new record
that supersedes the old one. **Proposed** means the owner has not accepted it: code may be written
in a way that keeps the choice open, never in a way that assumes the answer.

## ADR-001: Wheeped is a fork of Pumpkin; the engine work is called Sonix
Status: accepted (owner, 2026-11-23)
Context: SourbyCraft (a Paper fork with region threading) plus a stack of roughly ninety Java plugins
per server is too heavy for the network's hardware. Pumpkin is a Minecraft server written in Rust,
GPL-3.0, under active development.
Decision: Create Wheeped as a fork of `Pumpkin-MC/Pumpkin` under `YanIanZ`, modify it heavily for
Wureta Network, and call the engine work Sonix.
Consequences: Wheeped is GPL-3.0. It inherits Pumpkin's state, including what is unfinished there.
Following upstream becomes a standing cost (ADR-003).

## ADR-002: Systems come from SourbyCraft; plugins are rewritten in Rust
Status: accepted (owner, 2026-11-23)
Context: The network depends on SourbyCraft's Aurora World Fabric and MultiProfile and on its plugin
stack. Wheeped cannot load Java plugins.
Decision: Port AWF, Profiles and the other SourbyCraft systems to Rust, provide an API plugins can
run against, and move the plugins in use to Rust.
Consequences: Every system and plugin the network needs must exist in Rust before the server that
needs it can move. The order is ADR-007.

## ADR-003: Fork discipline
Status: proposed
Context: Pumpkin merges many changes a day. A fork that edits upstream files freely stops being able
to merge.
Decision: `master` mirrors upstream and takes no Wheeped commits. `main` is Wheeped. New code lives
in `crates/sonix-*`. Pumpkin's files are edited only at narrow hook points, each listed in
`UPSTREAM.md` with its reason. Crate, package and binary names stay `pumpkin-*`. Upstream is merged
into `main` through a pull request.
Consequences: The product name lives in strings and documents, not in crate names. Renaming a crate
later is possible but costs a conflict on every file that imports it.

## ADR-004: First-party plugins are native Rust, built with the server
Status: proposed
Context: Pumpkin offers native plugins (a dynamic library with the server's own types, no stable
ABI) and Wasm plugins (sandboxed, stable interface, a narrower API and a call boundary). The goal of
the move is speed, and the owner builds both server and plugins.
Decision: Wureta's plugins are native crates in one workspace with the server, compiled by the same
toolchain from the same commit, against Pumpkin's native plugin API plus `sonix-api`. Wasm remains
for plugins that should be sandboxed.
Consequences: A native plugin must be rebuilt with every server build; the loader already refuses a
mismatched `PLUGIN_API_VERSION`, but same-version builds from different commits are not safe to mix.
A crash in a native plugin is a crash of the server.

## ADR-005: The AWF store keeps SourbyCraft's on-disk format
Status: proposed
Context: The network's island worlds and templates are AWF stores written by SourbyCraft. Even if
Wheeped ended up with another native format, an importer would have to read these.
Decision: `sonix-awf` reads and writes the FILE backend's format: objects, generations, manifest
formats 1 and 2, index and delta blobs, `.awf` version 1 and 2 files. The format, not every byte:
pointer, manifest, index and delta files come out identical, while an object's compressed bytes may
differ because the two zlib implementations compress differently. Nothing in the format depends on
that: an object is named by the hash of its uncompressed bytes.
Consequences: A store can be opened by either server, and each can commit on top of the other's
generations. Compatibility is at the storage layer only: the chunk payload is vanilla NBT of the
writer's Minecraft version (26.2 for SourbyCraft), and loading it on 26.3 is separate work that
nobody has shown to be possible yet.

## ADR-006: A Java bridge only as a transition aid
Status: proposed
Context: PatchBukkit runs Bukkit, Spigot and Paper plugins on Pumpkin by embedding a JVM with a
headless Paper server. Nobody on this project has run Wureta's plugins on it.
Decision: Evaluate it for plugins that are not ported yet. Do not design Sonix around it.
Consequences: If it works for a plugin, that plugin's port can come later. It keeps the JVM's cost,
so it does not serve the reason for the move.

## ADR-007: Servers move one at a time, simplest first
Status: proposed
Context: The network must stay up. Servers differ in how much vanilla behaviour and how many plugins
they need.
Decision: Move the fallback (limbo) server first, then the lobby, then the event and dungeon
servers, and the island server last.
Consequences: Proxy forwarding, authentication handoff and the cross-server data path are proven on
a server with almost no gameplay. The island server waits for the most: vanilla mechanics, AWF,
Profiles and the SkyBlock plugins.

## ADR-008: Wureta's plugin ports live in a private repository
Status: proposed
Context: This repository is public. The network's plugin inventory, configuration and ported
commercial functionality are not for publication.
Decision: The engine and its API are developed here. Wureta's plugins are developed in a private
repository that depends on this one.
Consequences: This repository never names the network's plugins, addresses or ids.

## ADR-009: Equivalence bar for ports
Status: proposed
Context: "It compiles" proves nothing about a port.
Decision: Storage and codec units match the Java original byte for byte, shown by fixtures written
by the Java code and read by the Rust code, and the reverse. Pure logic matches on recorded cases.
Gameplay a player can see is checked by the owner in game.
Consequences: A unit is `done` in `PORT-MAP.md` only with its evidence named. Until then it is
`ported, unverified`.

## ADR-010: Third-party plugins: port open source, re-create closed source
Status: proposed
Context: The plugin stack mixes open-source plugins, the owner's own plugins and commercial
closed-source plugins.
Decision: An open-source plugin is ported under its licence, with attribution, when the licence is
compatible with GPL-3.0. A closed-source plugin's features are written anew from its observable
behaviour, configuration and data formats; decompiled code is not copied into the port.
Consequences: Each plugin's licence is recorded before work starts. Some features will be simpler
than the original in their first version.

## ADR-011: Names
Status: proposed
Context: AWF and Profiles are known names on the network; Aurora is SourbyCraft's engine name.
Decision: Keep "AWF" and "Profiles" as subsystem names and `.awf` as the file suffix. Crates are
`sonix-awf`, `sonix-profiles`, `sonix-api`. Build numbering and codenames are the owner's to set.
Consequences: Documents can refer to SourbyCraft's AWF documentation without translating terms.
