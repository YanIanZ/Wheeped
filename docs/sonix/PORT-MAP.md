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
| `ChunkKey` | 25 | chunk position, ordering, packing | `key` | done ¹ |
| `ChunkSource`, `LayeredSource` | 44 | where serialized chunks are read from | `source` | done ¹ |
| `WorldRole`, `PersistenceMode` | 60 | roles and commit modes | `role` | done ¹ |
| `Durability` | 130 | file and directory forces, atomic rename | `durability` | done ¹ |
| `AwfFile` | 260 | `.awf` version 1 image, and its deflate codec | `image`, `codec` | done ¹ ² |
| `ObjectStore` | 272 | content-addressed chunk objects | `object_store` | done ¹ ² |
| `GenerationStore` | 598 | atomic generation commits, manifests 1 and 2 | `generation` | done ¹ ² |
| `AwfStore` | 69 | the contract every backend keeps | `store` | done ¹ |
| `AwfWorldStore` | 613 | chunk index and deltas over the two stores | `world_store` | done ¹ ² |
| `Compression` | 92 | codec ids and zstd, used by the version 2 world file | `codec` | done ¹ ² |
| `AwfWorldFile` | 288 | `.awf` version 2 portable world file | `world_file` | done ¹ ² |
| `AwfWorld` | 574 | copy-on-write world, save pipeline | `world`, `commit_lock` | ported, unverified ³ |
| `AwfMetrics`, `LatencyRecorder` | 67 | per-world counters and latency percentiles | `metrics` | ported, unverified ³ |
| `AwfShutdownBudget`, `AwfDeadlineExceeded` | 159 | the one time budget every wait of a shutdown shares, and the error of a call it cut off | `shutdown`, `error` | ported, unverified ³ |
| `AwfBackend` | 128 | backend selection | to be placed | open |
| `AwfRegistry` | 32 | a name-keyed set of worlds | none | not ported: nothing in SourbyCraft's runtime registers a world in it or reads it, as its own comment says; only a test does |

¹ Unit tests in the module, ported from the Java tests of the same unit (`GenerationStoreTest`,
`AwfWorldStoreTest`, `AwfFileTest`, `AwfWorldFileTest`, `AwfDeltaGenerationTest`, `AwfRollbackTest`, and
the store-level cases of `AwfDurabilityTest` and `AwfCompactionLifecycleTest`): 90 tests.

² Held against the Java original in both directions on 2026-11-23, as
`crates/sonix-awf/tests/fixtures/README.md` describes: Rust reads stores, an image and world files
written by the Build 57 classes (10 tests, part of `cargo test`), and the Java classes opened and
verified what Rust wrote and committed on top of its stores (run by hand, needs a JVM).

³ Written on 2026-11-23 with 47 unit tests: the Java tests of the same units ported
(`AwfWorldTest`, `AwfCommitOrderTest`, and the cases of `AwfConcurrentFlushTest`,
`AwfChunkUnloadRetentionTest`, `AwfLifecycleFenceTest`, `AwfShutdownDeadlineTest` and
`AwfDirtyBytesCommitTest` that are about the world itself, which the Java tests partly reach through
the region storage), and cases of this port's own for what those leave out. It compiles and passes
the lints. The tests have not been run yet, and nothing of it was held against the Java original.

What "done" does not cover: nothing in the server calls this crate yet, no world has been loaded
through it, and no store from a live server has been opened with it.

### Differences from the Java original

Deliberate, and small. Each is a place where the two would treat a damaged or unusual input
differently, or where a Java-only concern has no counterpart yet.

| Where | Java | Rust | Why |
| :-- | :-- | :-- | :-- |
| Numbers in `CURRENT`, manifests and generation directory names | any Unicode decimal digit (`Character.isDigit`) | ASCII digits only | No writer produces others; refusing them is the stricter reading of a damaged file |
| Chunk coordinates in an index | `Integer.parseInt`, which also takes non-ASCII digits | `i32` parsing, ASCII only | Same |
| Blocking I/O on a tick thread | every blocking call refuses to run on a region thread (by thread name) | no check | The check is tied to SourbyCraft's thread names; what replaces it is decided at the engine attach (wave 1) |
| `GenerationStore.commitAsync` | commits on an executor | not ported | Callers run the blocking commit on the pool they already have |
| Compressed bytes | `java.util.zip.Deflater` | a pure Rust zlib implementation | Both are zlib streams; each side inflates the other's |
| Error text for a damaged object | `chunk null ...` | `object <name> ...` | The Java text is an accident of passing no key |
| `Image::open` on a metadata string that is not modified UTF-8 | `UTFDataFormatException` | reported as corrupt | Both refuse the file |
| `CommitResult.generation` | `long`, `-1` when the world layer had nothing to commit | `u64` | The `-1` case belongs to the world layer, where a save with nothing to commit answers `None` |
| `AwfWorld.save(mode, executor, attempts)` | hands the commit to an executor and returns a `CompletionStage` | `World::save` returns a `QueuedSave`, and the caller's storage lane runs it | No executor or future type is tied into the crate. A queued save dropped without running frees its slot and counts as a failed save, which is what an executor refusing the task does in Java |
| A world's chunks in memory | `ConcurrentHashMap`s; a write is one `compute` | one mutex per world over the owned chunks and the dirty set | A chunk's bytes and its dirty state still change together. No I/O and no byte copy happens under the mutex; whether a world with many writers at once needs more than that is unmeasured |
| Bytes handed to a commit | the world's own arrays | a copy of each dirty chunk, made outside the mutex | `Store::commit` takes owned vectors. Sharing the bytes instead would remove the copy and is a change to the store contract |
| `LatencyRecorder` | an array of 1,024 samples per recorder from the start | grows to 1,024 samples | Same percentiles; a world that never commits holds no samples |
| A failing expiry action of a shutdown budget | a `RuntimeException` is logged and the next action runs | a panic is caught and logged, and the next action runs | Same intent: one failing action does not keep the others from running |
| Time values of a shutdown budget | milliseconds and nanoseconds as `long`, from a `LongSupplier` | `Duration`, from a closure returning nanoseconds | The deadline itself stays a nanosecond reading of that clock, for a backend that compares its own |
| `AwfShutdownBudget.tryLock(Lock)` | on the budget, for any lock | on the world's commit lock, private to the crate | That lock is the only one taken this way so far |
| `AwfDeadlineExceeded` with a cause | keeps the cause | no cause yet | It comes with the network backend, the only code that has one |
| `AwfWorld.closeStore`, `fence`, `fenced`, `commitRunning`, `uncertainChunks`, `dirtySample`, `oldestDirtyMillis` | package-private | public | The engine attach will not live in this module |
| zstd | zstd-jni (the C library) at level 3, and only where the server has it, else deflate | a pure Rust codec at its fastest level, always present | Both write standard frames and each side decodes the other's; compressed sizes differ, and the default codec of a world file writer is always zstd here |
| Corrupt zstd data, a duplicate or oversized chunk handed to a world file writer | plain `IOException` | `Corrupt`, or an I/O error of kind invalid input | Same family: the file or the call is refused |

### Found in the Java original while porting

Not fixed here: the port keeps the original's behaviour, and changing it is the owner's call.

- **A generation whose commit failed can later count as committed.** If a commit fails after its
  directory is published under its final name (a failed force of `generations/`, or any failure
  around the pointer) and the server keeps running, the next commit takes the following number and
  then treats the leftover directory as a committed generation: it takes a place in retention, so a
  real generation is deleted one commit early, and it is listed as a rollback point although its
  objects were collected. Reproduced on the Build 57 classes on 2026-11-23 with retention 2:
  commit 1 succeeds, commit 2 fails at the published stage, commit 3 succeeds; the retained
  generations are then 3 and the failed 2, generation 1 is gone, and a rollback to 2 fails with a
  missing object. A restart does not clear it, because the leftover's number is below the current
  one. Removing uncommitted generation directories at the start of a commit, as an open already
  does, would close it.
- **A chunk above 64 MiB can be written but never read.** `ObjectStore.put` takes any chunk an `int`
  can count, while `get` refuses an object that declares more than 64 MiB. A commit reads its own
  objects back before the commit point, so such a chunk is not stored unreadably: it fails every
  commit of its world from then on. No chunk comes near that size. The port keeps both limits.
- **Cleaning an object directory follows a symbolic link.** A writable open and `retainOnly` delete
  files in every directory under `objects/`, and take a symbolic link to a directory for one.
  Whoever can plant a link there can already write the store, so this is hardening rather than a
  hole. The port does the same.

## Wave 1: attach to the engine

Depends on wave 0 and on Pumpkin's chunk I/O.

| Unit (Java) | Lines | What it is | State |
| :-- | --: | :-- | :-- |
| `AwfChunkNbt` | 31 | chunk tag to stored bytes (vanilla uncompressed named-root NBT) | open |
| `AwfRegionStorage`, `RegionFileStorageHooks`, `AwfWorldFolders` | 1,085 | the storage a world's chunks go through | open: redesigned for `pumpkin-world`'s chunk I/O, not transliterated |
| `AwfEngine`, `AwfSettings` | 1,129 | which worlds are managed, store lifecycle, settings | open |
| `AwfMaintenanceDriver`, `AwfFlushReporting`, `AwfShutdownReport` | 588 | periodic commits, flush and shutdown reporting (the shutdown budget itself is in wave 0, because the world takes it) | open |
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

1. Can a chunk saved by a 26.2 server be loaded by the 26.3 base? A first reading, 2026-11-23, with
   a scratch test that is not in the repository: the overworld region store of a SourbyCraft 26.2
   test server (529 chunks, data version 4903 against the base's 5023) opened in `sonix-awf` with
   every object verified, and the base's chunk parser (`ChunkData::internal_from_bytes`) took all
   529 without an error. A second reading the same day, of the Anvil region files of an older and
   larger world (7,548 chunks, 3,151 of them fully generated, 316 block entities, data versions 4440,
   4556 and 4790): the parser took every chunk as well. That shows the bytes parse. It does not show
   that such a world plays correctly: no data upgrade was applied, what the parser does with blocks
   or fields that changed between versions was not looked at, and nothing was loaded into a running
   server.
2. A Wureta island server holds one world per island. Several server-wide lookups in the base walk
   the world list. How that behaves with thousands of worlds is unmeasured.
3. Paper serializes an item with `ItemStack.serializeAsBytes`; MultiProfile stores those bytes as
   opaque. Reading them into the base's item model needs a converter that does not exist.
