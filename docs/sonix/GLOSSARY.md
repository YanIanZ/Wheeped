# Glossary

One name per thing, the same in every unit. The Java name is the one in SourbyCraft; the Rust name
is the one in `crates/sonix-*`. Add a term when a unit brings it.

## Products

| Term | Meaning |
| :-- | :-- |
| Wheeped | This server: Pumpkin plus Sonix. |
| Sonix | The engine work done in this fork. |
| Pumpkin | The upstream server Wheeped is forked from. |
| SourbyCraft | The owner's Paper fork; the Java originals are ported from it. |
| Aurora | SourbyCraft's engine name. Used here only when pointing at SourbyCraft. |

## Aurora World Fabric (AWF)

| Term | Meaning | Java | Rust |
| :-- | :-- | :-- | :-- |
| chunk key | A chunk's position. Ordered by `z`, then `x`. | `ChunkKey` | `ChunkKey` |
| chunk bytes | One chunk as stored: vanilla uncompressed named-root NBT. The store treats them as opaque. | `byte[]` | `Vec<u8>` / `&[u8]` |
| chunk source | Anything serialized chunks can be read from. | `ChunkSource` | `ChunkSource` |
| layered source | An upper source over a lower one; the upper wins. | `LayeredSource` | `LayeredSource` |
| object | A chunk's deflated bytes, named by the SHA-256 of the uncompressed bytes. | `ObjectStore` | `ObjectStore` |
| object name | 64 lower-case hex characters. Nothing else is ever turned into a path. | `ObjectStore.NAME` | `ObjectName` |
| generation | One committed snapshot of a store's blobs, numbered from 1. | `GenerationStore.Generation` | `Generation` |
| manifest | A generation's list of blobs with length and hash; format 2 adds a base. | `manifest` | `Manifest` |
| base | The earlier generation a delta generation is chained to. | `@base` | `base` |
| `CURRENT` | The pointer file naming the committed generation. Replacing it is the commit point. | `CURRENT` | `CURRENT` |
| index | The full map from chunk key to object name for one generation. | `index` blob | `index` |
| delta | The entries that changed against the base generation. | `delta` blob | `delta` |
| tombstone | Index value `-`: the chunk was deleted and shadows the base. | `TOMBSTONE` | `Entry::Deleted` |
| entry | What an index says about one chunk: an object, or a deletion. | index value (`String`) | `Entry` |
| drop | Delta value `drop`: the chunk leaves the index and the base shows through again. | `DROP` | `Change::Drop` (private) |
| retention | How many committed generations are kept as rollback points. | `retainedGenerations` | `retained_generations` |
| compaction | Writing a full index to end a delta chain. | `compact()` | `compact()` |
| world store | A world's committed chunks: an index per generation plus objects. | `AwfWorldStore` | `WorldStore` |
| store | The contract every backend keeps (atomic commit, remembered deletions). | `AwfStore` | `Store` |
| image | The `.awf` version 1 file: one immutable chunk stream with SHA-256. | `AwfFile` | `Image` |
| world file | The `.awf` version 2 file: a whole portable world, several streams, CRC32C. | `AwfWorldFile` | `WorldFile`, `WorldFileWriter` |
| stream | One named set of chunks in a world file: `region`, `entities`, `poi`. | stream name | `WorldFileStream` |
| codec | How one chunk of a world file is stored: none, deflate or zstd. | `Compression` ids | `Codec` |
| world role | `VANILLA`, `VIRTUAL`, `TEMPLATE`, `INSTANCE`, `READ_ONLY`, `TEMPORARY`. | `WorldRole` | `WorldRole` |
| persistence mode | `FULL`, `INCREMENTAL`, `CHECKPOINT`, `READ_ONLY`. | `PersistenceMode` | `PersistenceMode` |
| durability | The seam for file force, directory force and atomic rename. | `Durability` | `Durability`, `Disk`, `DISK` |
| stage | A point in the commit sequence where a test can inject a crash. | `GenerationStore.Stage` | `Stage`, `StageHook` |
| compaction policy | Whether commits may write deltas, and after how many hops a full index is written. | `AwfWorldStore.Compaction` | `Compaction` |
| template | An immutable store that instances share. | `TEMPLATE` role | same |
| instance | A copy-on-write world over a template. | `INSTANCE` role | same |

## MultiProfile

Terms are added when wave 3 is planned. Until then SourbyCraft's `docs/awf/multiprofile/` is the
reference.
