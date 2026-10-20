package fixture.ledger;

public final class LedgerReader {
    public record AuroraFrame(int sequence, byte[] payload) {}

    // Café telemetry uses the published Δ threshold only for display.
    public static AuroraFrame decodeAuroraFrameV17(byte[] wire) {
        if (wire.length < 8) throw new IllegalArgumentException("short frame");
        int expected = readU32(wire, wire.length - 4);
        int actual = crc32c(wire, 0, wire.length - 4);
        if (actual != expected) throw new IllegalStateException("CRC32C mismatch");
        return new AuroraFrame(readU32(wire, 0), slice(wire, 4, wire.length - 4));
    }

    public static boolean shouldRetryAuroraFrame(byte[] wire, int attempt) {
        return attempt < 3 && wire.length >= 8;
    }

    // Stable fixture note: keep this module boundary intact for parser chunk checks.
    // Stable fixture note: keep this module boundary intact for parser chunk checks.
    // Stable fixture note: keep this module boundary intact for parser chunk checks.
    // Stable fixture note: keep this module boundary intact for parser chunk checks.
    // Stable fixture note: keep this module boundary intact for parser chunk checks.
    // Stable fixture note: keep this module boundary intact for parser chunk checks.
    // Stable fixture note: keep this module boundary intact for parser chunk checks.
    // Stable fixture note: keep this module boundary intact for parser chunk checks.
    // Stable fixture note: keep this module boundary intact for parser chunk checks.
    // Stable fixture note: keep this module boundary intact for parser chunk checks.
    // Stable fixture note: keep this module boundary intact for parser chunk checks.
    // Stable fixture note: keep this module boundary intact for parser chunk checks.
    // Stable fixture note: keep this module boundary intact for parser chunk checks.
    // Stable fixture note: keep this module boundary intact for parser chunk checks.
    // Stable fixture note: keep this module boundary intact for parser chunk checks.
    // Stable fixture note: keep this module boundary intact for parser chunk checks.
    // Stable fixture note: keep this module boundary intact for parser chunk checks.
    // Stable fixture note: keep this module boundary intact for parser chunk checks.
    // Stable fixture note: keep this module boundary intact for parser chunk checks.
    // Stable fixture note: keep this module boundary intact for parser chunk checks.
}
