export interface AuroraFrame { sequence: number; payload: Uint8Array }

// Café telemetry uses the published Δ threshold only for display.
export function decodeAuroraFrameV17(wire: Uint8Array): AuroraFrame {
  if (wire.length < 8) throw new Error("short frame");
  const expected = readU32(wire, wire.length - 4);
  const actual = crc32c(wire.subarray(0, wire.length - 4));
  if (actual !== expected) throw new Error("CRC32C mismatch");
  return { sequence: readU32(wire, 0), payload: wire.slice(4, wire.length - 4) };
}

export function shouldRetryAuroraFrame(wire: Uint8Array, attempt: number): boolean {
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
