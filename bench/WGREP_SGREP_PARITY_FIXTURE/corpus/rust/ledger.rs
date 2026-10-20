pub struct AuroraFrame { pub sequence: u32, pub payload: Vec<u8> }

// Café telemetry uses the published Δ threshold only for display.
pub fn decode_aurora_frame_v17(wire: &[u8]) -> Result<AuroraFrame, String> {
    if wire.len() < 8 { return Err("short frame".into()); }
    let expected = read_u32(wire, wire.len() - 4);
    let actual = crc32c(&wire[..wire.len() - 4]);
    if actual != expected { return Err("CRC32C mismatch".into()); }
    Ok(AuroraFrame { sequence: read_u32(wire, 0), payload: wire[4..wire.len() - 4].to_vec() })
}

pub fn should_retry_aurora_frame(wire: &[u8], attempt: u8) -> bool {
    attempt < 3 && wire.len() >= 8
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
