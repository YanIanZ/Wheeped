# Fixtures written by the Java original

`java-b57/` holds two world stores and one image written by SourbyCraft's own AWF classes at
Build 57 (`release/b57`, commit `0d2b7e39`), through the driver in `../java/Fixtures.java`.
`tests/java_fixtures.rs` reads them on every `cargo test`. They are the evidence that the port
opens what the Java store writes; do not edit them by hand. The files are hash-linked: `CURRENT`
names a manifest's SHA-256 and a manifest names each blob's, so a changed byte fails the tests.
`.gitattributes` here keeps git from converting line endings for that reason.

| Fixture | What it is |
| :-- | :-- |
| `java-full/` | Three generations of full indexes (retention 3): an empty chunk, a 20,000-byte chunk, corner coordinates, a rewrite, a removal, two deletions, a checkpoint commit |
| `java-deltas/` | A full index and three deltas on top (retention 2, delta generations on): changes, a drop, tombstones, a tombstone written again, a tombstone dropped |
| `java-image.awf` | A version 1 image: metadata with non-ASCII text, a NUL and keys that order differently in UTF-16 and UTF-8; an empty chunk and corner coordinates |
| `expected.tsv` | What the Java classes read back from the three, one fact per line |

## Regenerating them

The store classes of SourbyCraft compile on their own; they need only `slf4j-api` on the class path.
`J` is `sourbycraft-server/src/main/java/dev/iyanz/sourbycraft` in a SourbyCraft checkout.

```sh
javac -d out -cp slf4j-api.jar \
  $J/awf/{AwfFile,ChunkKey,ChunkSource,ObjectStore,GenerationStore,Durability,AwfWorldStore,AwfStore,WorldRole,PersistenceMode}.java \
  $J/execution/ExecutionLane.java crates/sonix-awf/tests/java/Fixtures.java
java -cp out:slf4j-api.jar dev.iyanz.sourbycraft.awf.Fixtures write DIR
```

Copy `java-full/`, `java-deltas/`, `java-image.awf` and `expected.tsv` from `DIR` to `java-b57/`
(or a new folder named after the build, with the tests pointed at it).

## The other direction: Java reading what Rust wrote

This needs a JVM and the SourbyCraft sources, so it is not part of `cargo test`. With the classes
compiled as above and an empty `DIR`:

```sh
SONIX_AWF_CROSS_DIR=DIR cargo test -p sonix-awf --test java_fixtures -- --ignored write_stores_for_the_java_check
java -cp out:slf4j-api.jar dev.iyanz.sourbycraft.awf.Fixtures verify DIR
SONIX_AWF_CROSS_DIR=DIR cargo test -p sonix-awf --test java_fixtures -- --ignored read_what_java_appended
```

The first command writes the same scenario from Rust. The second opens those stores with the Java
classes, read-only and then for writing, compares every chunk and deletion, and commits one more
generation on each. The third reads that generation back from Rust.

Last run: 2026-11-23, macOS arm64, OpenJDK 25.0.2, rustc 1.99.0. All three steps passed. For the
same scenario every file under `generations-root/` (pointer, manifests, index and delta blobs) was
byte for byte the same from both writers, as was the image. Of 38 objects, 37 were identical and
one differed only in its compressed bytes: the two zlib implementations compress differently, and
the format does not depend on it.
