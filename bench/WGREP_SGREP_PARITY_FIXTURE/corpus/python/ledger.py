from dataclasses import dataclass


@dataclass
class AuroraFrame:
    sequence: int
    payload: bytes


# Café telemetry uses the published Δ threshold only for display.
def decode_aurora_frame_v17(wire: bytes) -> AuroraFrame:
    if len(wire) < 8:
        raise ValueError("short frame")
    expected = read_u32(wire, len(wire) - 4)
    actual = crc32c(wire[:-4])
    if actual != expected:
        raise ValueError("CRC32C mismatch")
    return AuroraFrame(read_u32(wire, 0), wire[4:-4])


def should_retry_aurora_frame(wire: bytes, attempt: int) -> bool:
    return attempt < 3 and len(wire) >= 8


# Stable fixture note: keep this module boundary intact for parser chunk checks.
# Stable fixture note: keep this module boundary intact for parser chunk checks.
# Stable fixture note: keep this module boundary intact for parser chunk checks.
# Stable fixture note: keep this module boundary intact for parser chunk checks.
# Stable fixture note: keep this module boundary intact for parser chunk checks.
# Stable fixture note: keep this module boundary intact for parser chunk checks.
# Stable fixture note: keep this module boundary intact for parser chunk checks.
# Stable fixture note: keep this module boundary intact for parser chunk checks.
# Stable fixture note: keep this module boundary intact for parser chunk checks.
# Stable fixture note: keep this module boundary intact for parser chunk checks.
# Stable fixture note: keep this module boundary intact for parser chunk checks.
# Stable fixture note: keep this module boundary intact for parser chunk checks.
# Stable fixture note: keep this module boundary intact for parser chunk checks.
# Stable fixture note: keep this module boundary intact for parser chunk checks.
# Stable fixture note: keep this module boundary intact for parser chunk checks.
# Stable fixture note: keep this module boundary intact for parser chunk checks.
# Stable fixture note: keep this module boundary intact for parser chunk checks.
# Stable fixture note: keep this module boundary intact for parser chunk checks.
# Stable fixture note: keep this module boundary intact for parser chunk checks.
# Stable fixture note: keep this module boundary intact for parser chunk checks.
