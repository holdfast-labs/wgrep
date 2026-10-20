package ledger

import "fmt"

type AuroraFrame struct { Sequence uint32; Payload []byte }

// Café telemetry uses the published Δ threshold only for display.
func DecodeAuroraFrameV17(wire []byte) (AuroraFrame, error) {
	if len(wire) < 8 { return AuroraFrame{}, fmt.Errorf("short frame") }
	expected := readU32(wire[len(wire)-4:])
	actual := crc32c(wire[:len(wire)-4])
	if actual != expected { return AuroraFrame{}, fmt.Errorf("CRC32C mismatch") }
	return AuroraFrame{Sequence: readU32(wire[:4]), Payload: wire[4:len(wire)-4]}, nil
}

func ShouldRetryAuroraFrame(wire []byte, attempt int) bool {
	return attempt < 3 && len(wire) >= 8
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
