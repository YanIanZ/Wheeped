# Sonix architecture

Sonix is the engine work Wheeped adds to Pumpkin: region-aware world storage, durable player
profiles, the services a network of servers needs, and the API first-party plugins use. This page
says what exists and what does not. Anything not marked *implemented* is a plan.

Last checked against the code: 2026-11-23, Pumpkin base `a49be4bee` (`0.2.2+26.3-26.63`).

## Where Wheeped starts from

These are facts about the Pumpkin base, read from the code on the date above. They are the ground
Sonix builds on, not Wheeped's work.

| Area | What the base does | Where |
| :-- | :-- | :-- |
| Protocol | Java 26.3, one version at a time; Bedrock 26.63 marked work in progress | `pumpkin-data` (`CURRENT_MC_VERSION`), `README` |
| Tick | Game logic is synchronous inside the tick. Worlds tick in parallel (`worlds.par_iter()`), players tick in parallel, and inside a world entities tick in parallel islands | `pumpkin-core/src/server/mod.rs`, `world/entity_islands.rs` |
| Async | Networking, disk I/O, plugins and events on Tokio; chunk generation and lighting on Rayon | Pumpkin's guide in `AGENTS.md` |
| Worlds | A list of worlds that can grow and shrink at runtime: `Server::create_world`, `Server::unload_world` (refuses the primary world and a world with players) | `pumpkin-core/src/server/mod.rs` |
| Chunk storage | Anvil, Linear and Pump formats behind one file I/O layer; Pump is the default | `pumpkin-world/src/chunk/format/`, `pumpkin-config/src/chunk.rs` |
| Plugins | Native plugins (a dynamic library exporting `PUMPKIN_API_VERSION`, `METADATA`, `plugin`) and Wasm plugins (API v0.1; v0.2 behind a feature) | `pumpkin-core/src/plugin/`, `pumpkin-wasm-host*` |
| Proxies | BungeeCord, BungeeGuard and Velocity forwarding | `pumpkin-config/src/networking/` |

Not established by anyone on this project yet, and so not claimed: how the base performs under
Wureta's load, how close it is to vanilla for the mechanics a SkyBlock network depends on (farms,
spawners, redstone, hoppers, mob AI), and whether worlds saved by SourbyCraft 26.2 load here.

## What Sonix adds

| Subsystem | Comes from (SourbyCraft) | Rust home | Status |
| :-- | :-- | :-- | :-- |
| AWF storage core: content-addressed objects, atomic generations, world store, `.awf` version 1 image | `awf/*.java` | `crates/sonix-awf` | **implemented**, held against the Java original; used by nothing in the server yet. See [PORT-MAP.md](PORT-MAP.md) |
| `.awf` version 2 world file and the zstd codec | `awf/AwfWorldFile.java`, `Compression.java` | `crates/sonix-awf` | planned |
| AWF world layer: copy-on-write worlds over templates, save pipeline | `awf/AwfWorld.java` | `crates/sonix-awf` | planned |
| Engine attach: chunks of a managed world read and written through AWF instead of region files | `AwfRegionStorage`, `AwfEngine`, feature patch 0021 | a chunk I/O backend in `pumpkin-world` | planned |
| Runtime worlds: create, load, save, unload, delete, templates, instances, registry | `awf/world/` (`AuroraWorlds`) | to be named | planned |
| Redis world backend | `awf/redis/` | to be named | planned |
| Redis Fabric: sessions, coordination, events, cache, routing, node identity | `awf/redis/fabric/` | to be named | planned |
| MultiProfile: profiles, snapshots, switching, co-op, bank, types, file and Redis repositories | `awf/multiprofile/` | `crates/sonix-profiles` | planned |
| AbyssLink: control link to the Leviathan proxy | `abysslink/` | to be named | planned |
| Plugin API for first-party plugins | `sourbyapi` | `crates/sonix-api` | planned, see ADR-004 |
| Observability (`/perf`), packet and connection guards, updater | `perf/`, `security/`, `update/` | to be decided per item: Pumpkin has its own profiler, guards and update checker | not started |

Sizes of the Java sources being ported are in [PORT-MAP.md](PORT-MAP.md). The largest is
MultiProfile, about 33,000 lines; the storage core is about 7,000.

## Aurora World Fabric in Wheeped

AWF is the world storage and instancing runtime: a world's chunks live in a store instead of region
files, a template is an immutable store, and an instance is a copy-on-write layer over a template.
The design is described in SourbyCraft's `docs/architecture/aurora-world-fabric.md`; the rules that
carry over unchanged are:

- **Atomic commits.** A save writes a new generation beside the old one, reads it back, and only
  then replaces the `CURRENT` pointer. A crash before that leaves the previous generation
  authoritative.
- **Content addressing.** A chunk's bytes are stored once under the SHA-256 of their uncompressed
  form, so an unchanged chunk costs nothing to save again and an index can never name a file outside
  its object directory.
- **Nothing is durable by accident.** Files are forced, then renamed, then their directory is
  forced. A filesystem that cannot rename atomically fails the commit; nothing is published by
  copying.
- **Deletions are remembered.** A deleted chunk is a tombstone that shadows the base, so it does not
  reappear from the template underneath.
- **Template isolation.** An instance never writes its template.

The Rust store reads and writes the Java store's on-disk format (ADR-005, proposed), and each side
has opened, verified and committed on top of stores the other wrote (`crates/sonix-awf/tests/fixtures/README.md`).
That makes the storage layer portable between the two servers. It does not make a 26.2 world
playable on a 26.3 server: the chunk payload inside each object is vanilla NBT of the Minecraft
version that wrote it, and upgrading that payload is a separate, unproven step.

What changes in Wheeped is the attach point. SourbyCraft hooks AWF under Paper's
`RegionFileStorage` and has to refuse blocking I/O on region threads. Here the store sits behind
`pumpkin-world`'s chunk I/O layer, whose reads and writes already run off the tick.

## Plugins

Wureta's plugins are rewritten in Rust (ADR-002). The proposed model (ADR-004) is native plugins
built in one workspace with the server, against Pumpkin's native plugin API plus a Sonix facade for
worlds, profiles and network services. Pumpkin's Wasm API stays available for plugins that should
be sandboxed.

Open and unproven: whether [PatchBukkit](https://github.com/Pumpkin-MC/PatchBukkit) can carry Java
plugins that are not ported yet (ADR-006). It embeds a JVM and a headless Paper server, so it is a
bridge for the transition at most, never the goal.

## What Wheeped changes in Pumpkin's own files

Kept small on purpose. The list, with the reason for each, is in [UPSTREAM.md](UPSTREAM.md).
