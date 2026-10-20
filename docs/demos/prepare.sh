#!/usr/bin/env bash
# Source this file from a VHS shell in the package root.
set -e
: "${WG_DEMO_WORK:?Set WG_DEMO_WORK to an empty task-owned directory}"
mkdir -p "$WG_DEMO_WORK"
test ! -e "$WG_DEMO_WORK/payments-service"
test ! -e "$WG_DEMO_WORK/search-service"
export WG_SERVICE_DIR="${WG_SERVICE_DIR:-$WG_DEMO_WORK/service}"
export HF_HUB_OFFLINE="${HF_HUB_OFFLINE:-1}"
export WG_DEMO_A="$WG_DEMO_WORK/payments-service"
export WG_DEMO_B="$WG_DEMO_WORK/search-service"
mkdir "$WG_DEMO_A" "$WG_DEMO_B"
cat > "$WG_DEMO_A/auth.rs" <<'EOF'
// Reject a request when its token is missing.
pub fn validate_token(token: &str) -> bool {
    !token.trim().is_empty()
}
EOF
cat > "$WG_DEMO_A/queue.rs" <<'EOF'
// Choose the next queued job.
pub fn next_job(queue: &mut Vec<String>) -> Option<String> {
    queue.pop()
}
EOF
cat > "$WG_DEMO_B/packet.rs" <<'EOF'
// Decode the packet length.
pub fn decode_packet(bytes: &[u8]) -> usize {
    bytes.len()
}
EOF
set +e
export PS1='\[\e[1;38;5;117m\]\W\[\e[0m\] \[\e[38;5;213m\]❯\[\e[0m\] '
trap 'wg down >/dev/null 2>&1 || true' EXIT
cd "$WG_DEMO_A"
clear
