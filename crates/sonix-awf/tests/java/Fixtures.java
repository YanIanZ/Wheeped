package dev.iyanz.sourbycraft.awf;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.HexFormat;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.SortedMap;
import java.util.TreeMap;
import java.util.TreeSet;

/**
 * Cross-check driver for the Rust port in {@code crates/sonix-awf}. It runs SourbyCraft's own AWF
 * classes, so it sits in their package and is compiled together with them; see
 * {@code tests/fixtures/README.md}.
 *
 * <pre>
 * write DIR    writes java-full/, java-deltas/, java-image.awf, java-world-*.awf and expected.tsv into DIR
 * verify DIR   checks rust-full/, rust-deltas/, rust-image.awf and rust-world-*.awf in DIR against
 *              rust-expected.tsv, then commits one more generation on each store for the Rust side
 *              to read back
 * </pre>
 */
public final class Fixtures {

    private static final List<String> FAILURES = new ArrayList<>();

    public static void main(final String[] args) throws Exception {
        final Path dir = Path.of(args[1]);
        switch (args[0]) {
            case "write" -> write(dir);
            case "verify" -> verify(dir);
            default -> throw new IllegalArgumentException(args[0]);
        }
        if (!FAILURES.isEmpty()) {
            FAILURES.forEach(System.err::println);
            System.exit(1);
        }
        System.out.println(args[0] + " ok");
    }

    static byte[] b(final String text) {
        return text.getBytes(StandardCharsets.UTF_8);
    }

    static ChunkKey k(final int x, final int z) {
        return new ChunkKey(x, z);
    }

    /** Bytes that do not compress to nothing, the same on both sides. */
    static byte[] pattern(final int seed, final int length) {
        final byte[] out = new byte[length];
        int state = seed;
        for (int i = 0; i < length; i++) {
            state = state * 1103515245 + 12345;
            out[i] = (byte) (state >>> 16);
        }
        return out;
    }

    private static void write(final Path dir) throws Exception {
        final StringBuilder expected = new StringBuilder();

        // A store of full indexes: every build of the Java store writes and reads this.
        final AwfWorldStore full = AwfWorldStore.open(dir.resolve("java-full"), WorldRole.VANILLA, 3);
        final Map<ChunkKey, byte[]> first = new HashMap<>();
        first.put(k(0, 0), b("a"));
        first.put(k(1, 0), new byte[0]);
        first.put(k(-3, 7), pattern(1, 20_000));
        first.put(k(Integer.MAX_VALUE, Integer.MIN_VALUE), b("corner"));
        first.put(k(5, 5), b("to-delete"));
        first.put(k(6, 6), b("to-remove"));
        first.put(k(7, 7), b("héllo \u0000 😀"));
        full.commit(first, Set.of(), PersistenceMode.INCREMENTAL);
        full.commit(Map.of(k(0, 0), b("a2")), Set.of(k(6, 6)), Set.of(k(5, 5), k(9, 9)), PersistenceMode.INCREMENTAL);
        full.commit(Map.of(k(8, 8), pattern(2, 5_000)), Set.of(), PersistenceMode.CHECKPOINT);
        describe(expected, "java-full", full);

        // A delta chain: a full index and three deltas on top, written with delta generations on.
        final AwfWorldStore deltas = AwfWorldStore.open(dir.resolve("java-deltas"), WorldRole.VANILLA, 2, true);
        final Map<ChunkKey, byte[]> world = new HashMap<>();
        for (int i = 0; i < 30; i++) world.put(k(i, 0), b("chunk-" + i));
        deltas.commit(world, Set.of(), PersistenceMode.INCREMENTAL);
        deltas.commit(Map.of(k(1, 0), b("changed-1"), k(100, 0), b("new")), Set.of(k(10, 0)), Set.of(k(20, 0)),
            PersistenceMode.INCREMENTAL);
        deltas.commit(Map.of(k(2, 0), b("changed-2")), Set.of(), Set.of(k(21, 0)), PersistenceMode.INCREMENTAL);
        deltas.commit(Map.of(k(20, 0), b("back")), Set.of(k(21, 0)), Set.of(), PersistenceMode.INCREMENTAL);
        describe(expected, "java-deltas", deltas);

        // A version 1 image.
        final Map<String, String> metadata = new HashMap<>();
        metadata.put("role", "TEMPLATE");
        metadata.put("environment", "NORMAL");
        metadata.put("naïve", "çà");
        metadata.put("nul", "a\u0000b");
        metadata.put("emoji", "😀");
        metadata.put("�-after-surrogates", "utf-16 order");
        metadata.put("😀-before-fffd", "utf-16 order");
        final SortedMap<ChunkKey, byte[]> chunks = new TreeMap<>();
        chunks.put(k(0, 0), b("zero"));
        chunks.put(k(-1, -1), pattern(3, 9_000));
        chunks.put(k(3, -3), new byte[0]);
        chunks.put(k(Integer.MIN_VALUE, Integer.MAX_VALUE), b("corner"));
        AwfFile.write(dir.resolve("java-image.awf"), metadata, chunks);
        try (AwfFile image = AwfFile.open(dir.resolve("java-image.awf"))) {
            describeImage(expected, "java-image.awf", image);
        }

        // Version 2 world files, one per codec. zstd needs zstd-jni on the class path.
        for (final byte codec : new byte[] {Compression.NONE, Compression.DEFLATE, Compression.ZSTD}) {
            final String name = "java-world-" + Compression.name(codec) + ".awf";
            try (AwfWorldFile.Writer writer = AwfWorldFile.writer(dir.resolve(name), codec)) {
                writer.add("region", k(0, 0), b("zero"));
                writer.add("region", k(-1, -1), pattern(4, 9_000));
                writer.add("region", k(3, -3), new byte[0]);
                writer.add("region", k(Integer.MIN_VALUE, Integer.MAX_VALUE), b("corner"));
                writer.add("entities", k(0, 0), b("cow"));
                writer.add("poi", k(2, 2), pattern(5, 300));
                final Map<String, String> worldMetadata = new HashMap<>();
                worldMetadata.put("format", "awf-world");
                worldMetadata.put("environment", "normal");
                worldMetadata.put("seed", "-42");
                worldMetadata.put("na\u00efve", "\u00e7\u00e0");
                worldMetadata.put("nul", "a\u0000b");
                worldMetadata.put("emoji", "\uD83D\uDE00");
                writer.finish(worldMetadata);
            }
            try (AwfWorldFile file = AwfWorldFile.open(dir.resolve(name))) {
                describeWorldFile(expected, name, file);
            }
        }

        Files.writeString(dir.resolve("expected.tsv"), expected.toString(), StandardCharsets.UTF_8);
    }

    private static void verify(final Path dir) throws Exception {
        final Map<String, List<String[]>> expected = new HashMap<>();
        for (final String line : Files.readAllLines(dir.resolve("rust-expected.tsv"), StandardCharsets.UTF_8)) {
            final String[] parts = line.split("\t", -1);
            expected.computeIfAbsent(parts[0], name -> new ArrayList<>()).add(parts);
        }

        for (final String name : List.of("rust-full", "rust-deltas")) {
            final boolean deltas = name.endsWith("deltas");
            // The retention each store was written with: it decides the rollback points.
            final int retained = deltas ? 2 : 3;
            // A reader first: it must be able to open what Rust wrote without changing it.
            checkStore(name, expected.get(name), AwfWorldStore.open(dir.resolve(name), WorldRole.READ_ONLY, retained, deltas));
            // Then a writer: recovery and the object scan run, and the state is still the same.
            final AwfWorldStore writer = AwfWorldStore.open(dir.resolve(name), WorldRole.VANILLA, retained, deltas);
            checkStore(name + " (writable)", expected.get(name), writer);
            final long before = writer.generation();
            final AwfStore.CommitResult appended = writer.commit(Map.of(k(1000, 1000), b("java-on-top")), Set.of(),
                PersistenceMode.CHECKPOINT);
            check(name + " appended generation", before + 1, appended.generation());
            check(name + " appended as", deltas ? "delta" : "full", writer.lastCommitCompacted() ? "full" : "delta");
        }

        try (AwfFile image = AwfFile.open(dir.resolve("rust-image.awf"))) {
            final StringBuilder actual = new StringBuilder();
            describeImage(actual, "rust-image.awf", image);
            final StringBuilder wanted = new StringBuilder();
            for (final String[] parts : expected.get("rust-image.awf")) wanted.append(String.join("\t", parts)).append('\n');
            check("rust-image.awf", sortedLines(wanted.toString()), sortedLines(actual.toString()));
        }

        for (final String codec : List.of("none", "deflate", "zstd")) {
            final String name = "rust-world-" + codec + ".awf";
            try (AwfWorldFile file = AwfWorldFile.open(dir.resolve(name))) {
                final StringBuilder actual = new StringBuilder();
                describeWorldFile(actual, name, file);
                final StringBuilder wanted = new StringBuilder();
                for (final String[] parts : expected.get(name)) wanted.append(String.join("\t", parts)).append('\n');
                check(name, sortedLines(wanted.toString()), sortedLines(actual.toString()));
            }
        }
    }

    private static void checkStore(final String label, final List<String[]> expected, final AwfWorldStore store)
        throws IOException {
        final StringBuilder actual = new StringBuilder();
        describe(actual, expected.get(0)[0], store);
        final StringBuilder wanted = new StringBuilder();
        for (final String[] parts : expected) wanted.append(String.join("\t", parts)).append('\n');
        check(label, sortedLines(wanted.toString()), sortedLines(actual.toString()));
    }

    private static TreeSet<String> sortedLines(final String text) {
        return new TreeSet<>(List.of(text.split("\n")));
    }

    private static void check(final String what, final Object expected, final Object actual) {
        if (!expected.equals(actual)) FAILURES.add(what + ": expected " + expected + " but was " + actual);
    }

    /** One line per fact about a store, in the form the Rust side writes and reads. */
    private static void describe(final StringBuilder out, final String name, final AwfWorldStore store) throws IOException {
        out.append(name).append("\tgeneration\t").append(store.generation()).append('\n');
        out.append(name).append("\tchain\t").append(store.chainLength()).append('\n');
        final StringBuilder points = new StringBuilder();
        for (final long point : store.rollbackPoints()) points.append(points.isEmpty() ? "" : ",").append(point);
        out.append(name).append("\trollback\t").append(points).append('\n');
        for (final ChunkKey key : store.keys()) {
            final byte[] raw = store.read(key).orElseThrow();
            out.append(name).append("\tchunk\t").append(key.x()).append('\t').append(key.z()).append('\t')
                .append(raw.length).append('\t').append(sha256(raw)).append('\n');
        }
        for (final ChunkKey key : store.deleted()) {
            out.append(name).append("\tdeleted\t").append(key.x()).append('\t').append(key.z()).append('\n');
        }
    }

    private static void describeImage(final StringBuilder out, final String name, final AwfFile image) throws IOException {
        for (final Map.Entry<String, String> entry : image.metadata().entrySet()) {
            out.append(name).append("\tmeta\t").append(hex(b(entry.getKey()))).append('\t').append(hex(b(entry.getValue())))
                .append('\n');
        }
        for (final ChunkKey key : image.keys()) {
            final byte[] raw = image.read(key).orElseThrow();
            out.append(name).append("\tchunk\t").append(key.x()).append('\t').append(key.z()).append('\t')
                .append(raw.length).append('\t').append(sha256(raw)).append('\n');
        }
    }

    private static void describeWorldFile(final StringBuilder out, final String name, final AwfWorldFile file)
        throws IOException {
        out.append(name).append("\tstreams\t").append(String.join(",", file.streamNames())).append('\n');
        for (final Map.Entry<String, String> entry : file.metadata().entrySet()) {
            out.append(name).append("\tmeta\t").append(hex(b(entry.getKey()))).append('\t').append(hex(b(entry.getValue())))
                .append('\n');
        }
        for (final String stream : file.streamNames()) {
            for (final ChunkKey key : file.stream(stream).keys()) {
                final byte[] raw = file.stream(stream).read(key).orElseThrow();
                out.append(name).append("\tschunk\t").append(stream).append('\t').append(key.x()).append('\t')
                    .append(key.z()).append('\t').append(raw.length).append('\t').append(sha256(raw)).append('\n');
            }
        }
    }

    private static String hex(final byte[] bytes) {
        return HexFormat.of().formatHex(bytes);
    }

    private static String sha256(final byte[] bytes) {
        try {
            return hex(MessageDigest.getInstance("SHA-256").digest(bytes));
        } catch (final java.security.NoSuchAlgorithmException impossible) {
            throw new IllegalStateException(impossible);
        }
    }
}
