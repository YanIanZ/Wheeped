# Port map: SourbyCraft systems to Rust

The plan and the claims for the Java to Rust port. It points to other files and copies nothing from
them. A session starts here, claims one unit, ports it, and records the evidence.

## Goal

Aurora World Fabric, MultiProfile and the services around them run inside Wheeped as Rust code that
behaves like the Java originals in SourbyCraft, with the Java kept only as reference.

- Source: `sourbycraft-server/src/main/java/dev/iyanz/sourbycraft/` in
  [SourbyCraft](https://github.com/YanIanZ/SourbyCraft), read at Build 57 (`release/b57`, `0d2b7e39`).
- Expected behaviour: the Java tests under `sourbycraft-server/src/test/java/dev/iyanz/sourbycraft/awf/`
  (38 files for the storage core, 40 for MultiProfile) and SourbyCraft's `docs/awf/` and
  `docs/architecture/aurora-world-fabric.md`.
- Decisions: [DECISIONS.md](DECISIONS.md). The ones this port leans on are ADR-005 (byte-compatible
  store), ADR-009 (equivalence bar) and ADR-011 (names); all three are still proposed.
- Names: [GLOSSARY.md](GLOSSARY.md).

## States

`open` · `claimed (<who>, <date>)` · `ported, unverified` (compiles, has unit tests, not yet held
against the Java original) · `done` (evidence named in the row) · `not ported` (with the reason).

## What the analysis says

`rust-anything analyze` over the `awf` package (2026-11-23): 135 files, 45,242 lines of code,
36 units, 21 ready and 15 caution, none blocked. Two things it could not see, so the plan below does
not take them from the tool:

- It put every unit in wave 0. Java classes in one package reference each other without an import,
  so the tool found no dependencies. The waves below come from reading the sources.
- It found no tests, because they live outside the analysed folder. They exist (see Goal).

Most units are stateful classes over files, sockets or the server, not functions of their
arguments. A generated function bridge with recorded cases fits few of them (the codecs), so the
proof for the rest is fixtures crossing between the two implementations (ADR-009).

Risks the analysis flags most often, to read at their line before porting a unit: string offsets
(Java indexes UTF-16), unordered iteration that reaches output, regular expressions, integer
semantics, and `null` standing for "absent".

## Wave 0: storage core (FILE backend)

No dependency on a running server. Target crate: `crates/sonix-awf`.

| Unit (Java) | Lines | What it is | Rust module | State |
| :-- | --: | :-- | :-- | :-- |
| `ChunkKey` | 25 | chunk position, ordering, packing | `key` | open |
| `ChunkSource`, `LayeredSource` | 44 | where serialized chunks are read from | `source` | open |
| `WorldRole`, `PersistenceMode` | 60 | roles and commit modes | `role` | open |
| `Compression` | 92 | codec ids, deflate, zstd | `codec` | open |
| `Durability` | 130 | file and directory forces, atomic rename | `durability` | open |
| `AwfFile` | 260 | `.awf` version 1 image | `image` | open |
| `ObjectStore` | 272 | content-addressed chunk objects | `object_store` | open |
| `GenerationStore` | 598 | atomic generation commits, manifests 1 and 2 | `generation` | open |
| `AwfStore` | 69 | the contract every backend keeps | `store` | open |
| `AwfWorldStore` | 613 | chunk index and deltas over the two stores | `world_store` | open |
| `AwfWorldFile` | 288 | `.awf` version 2 portable world file | `world_file` | open |
| `AwfWorld` | 574 | copy-on-write world, save pipeline, metrics | `world` | open |
| `AwfBackend`, `AwfRegistry`, `AwfMetrics`, `LatencyRecorder` | 227 | backend selection and counters | to be placed | open |

## Wave 1: attach to the engine

Depends on wave 0 and on Pumpkin's chunk I/O.

| Unit (Java) | Lines | What it is | State |
| :-- | --: | :-- | :-- |
| `AwfChunkNbt` | 31 | chunk tag to stored bytes (vanilla uncompressed named-root NBT) | open |
| `AwfRegionStorage`, `RegionFileStorageHooks`, `AwfWorldFolders` | 1,085 | the storage a world's chunks go through | open: redesigned for `pumpkin-world`'s chunk I/O, not transliterated |
| `AwfEngine`, `AwfSettings` | 1,129 | which worlds are managed, store lifecycle, settings | open |
| `AwfMaintenanceDriver`, `AwfFlushReporting`, `AwfShutdownBudget`, `AwfShutdownReport`, `AwfDeadlineExceeded` | 747 | periodic commits, flush and shutdown deadlines | open |
| `ChunkPruning` | 127 | dropping empty chunks from a save | open |
| `ReadFailureLog`, `RepeatedReadFailureLogFilter` | 307 | rate-limited read failure logging | open |
| `world/` (11 files) | 3,260 | runtime worlds: create, load, save, unload, delete, templates, instances, registry, Slime import | open |

## Wave 2: network backends

| Unit (Java) | Lines | What it is | State |
| :-- | --: | :-- | :-- |
| `redis/` (4 files) | 1,701 | Redis world backend and client | open |
| `redis/fabric/` (20 files) | 2,816 | sessions, coordination, events, cache, routing, node identity | open |

## Wave 3: MultiProfile

| Unit (Java) | Lines | What it is | State |
| :-- | --: | :-- | :-- |
| `multiprofile/` (68 files) | 30,787 | profiles, snapshots, switching, co-op, bank, types, audit, file and Redis repositories | open: to be split into units before any claim |

`ProfileEngine` alone is about 7,000 lines and the two repositories about 3,500 each. This wave is
planned per record type and per operation, not per file, once waves 0 to 2 stand.

## Wave 4: outside the `awf` package

Not analysed yet: `abysslink/` (the proxy control link), `security/` and `guards/`, `perf/`,
`update/`, `config/`, and the public API in `sourbyapi`. Each needs its own look at what Pumpkin
already provides before a port is planned.

## Not ported

- `execution/`, `bridge/`, `scheduler/` and the region-threading engine: they exist to run Bukkit
  plugins and Paper's world on region threads. Pumpkin has its own tick and scheduling model.
- `bootstrap/`, `startup/`, `mod/`, `brand/`: launcher, slim-jar and JVM concerns.

## Open questions

1. Can a chunk saved by a 26.2 server be loaded by the 26.3 base? Nothing here answers that yet.
2. A Wureta island server holds one world per island. Several server-wide lookups in the base walk
   the world list. How that behaves with thousands of worlds is unmeasured.
3. Paper serializes an item with `ItemStack.serializeAsBytes`; MultiProfile stores those bytes as
   opaque. Reading them into the base's item model needs a converter that does not exist.
