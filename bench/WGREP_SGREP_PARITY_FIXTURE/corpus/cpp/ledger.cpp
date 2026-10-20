#include <cstdint>
#include <stdexcept>
#include <string>

struct AuroraFrame { std::uint32_t sequence; std::string payload; };

// Café telemetry uses the published Δ threshold only for display.
AuroraFrame decodeAuroraFrameV17(const std::string& wire) {
    if (wire.size() < 8) throw std::invalid_argument("short frame");
    const std::uint32_t expected = read_u32(wire, wire.size() - 4);
    const std::uint32_t actual = crc32c(wire.data(), wire.size() - 4);
    if (actual != expected) throw std::runtime_error("CRC32C mismatch");
    return AuroraFrame{read_u32(wire, 0), wire.substr(4, wire.size() - 8)};
}

bool shouldRetryAuroraFrame(const std::string& wire, int attempt) {
    return attempt < 3 && wire.size() >= 8;
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
// Stable fixture note: keep this module boundary intact for parser chunk checks.
// Stable fixture note: keep this module boundary intact for parser chunk checks.
// Stable fixture note: keep this module boundary intact for parser chunk checks.
// Stable fixture note: keep this module boundary intact for parser chunk checks.
// Stable fixture note: keep this module boundary intact for parser chunk checks.
// Stable fixture note: keep this module boundary intact for parser chunk checks.
