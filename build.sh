#!/bin/bash
# build.sh - Static cross-builder (x86_64 + RPi 5) and direct 3-file SSH deployer

set -euo pipefail

################################################################################
# Paths, Constants & Environment
################################################################################

PROJECT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly PROJECT_DIR
readonly CARGO_MANIFEST="${PROJECT_DIR}/Cargo.toml"

readonly TARGET_X86="x86_64-unknown-linux-musl"
readonly TARGET_RPI5="aarch64-unknown-linux-musl"
readonly LABEL_X86="x86_64"
readonly LABEL_RPI5="rpi5"

if [[ -d "${HOME}/.cargo/bin" && ":${PATH}:" != *":${HOME}/.cargo/bin:"* ]]; then
    export PATH="${HOME}/.cargo/bin:${PATH}"
fi

################################################################################
# Console Formatting (NO_COLOR & TTY-aware on stderr)
################################################################################

if [[ -t 2 && -z "${NO_COLOR:-}" && "${TERM:-}" != "dumb" ]]; then
    readonly C_GREEN=$'\033[32m'
    readonly C_BLUE=$'\033[34m'
    readonly C_RED=$'\033[31m'
    readonly C_BOLD=$'\033[1m'
    readonly C_RESET=$'\033[0m'
else
    readonly C_GREEN=""
    readonly C_BLUE=""
    readonly C_RED=""
    readonly C_BOLD=""
    readonly C_RESET=""
fi

log_header() {
    printf '\n%s═══ %s ═══%s\n\n' "${C_BOLD}" "$1" "${C_RESET}" >&2
}

log_info() {
    printf '%sℹ%s %s\n' "${C_BLUE}" "${C_RESET}" "$1" >&2
}

log_ok() {
    printf '%s✓%s %s\n' "${C_GREEN}" "${C_RESET}" "$1" >&2
}

log_err() {
    printf '%s✗%s %s\n' "${C_RED}" "${C_RESET}" "$1" >&2
}

################################################################################
# Metadata Extraction & Tooling Pre-Checks
################################################################################

read_cargo_field() {
    local field="$1"
    local value
    value="$(sed -nE '/^\[package\]/,/^\[/{s/^[[:space:]]*'"${field}"'[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p;}' "${CARGO_MANIFEST}" | head -n 1)"
    if [[ -z "${value}" ]]; then
        log_err "Could not parse '${field}' from ${CARGO_MANIFEST}"
        exit 1
    fi
    printf '%s\n' "${value}"
}

require_cmd() {
    local cmd="$1"
    local hint="$2"
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        log_err "Missing required command '${cmd}'. ${hint}"
        exit 1
    fi
}

verify_build_toolchain() {
    require_cmd "cargo" "Install Rust via https://rustup.rs"
    require_cmd "zig" "Install zig (e.g., sudo dnf install -y gcc zig)"
    require_cmd "cargo-zigbuild" "Run: cargo install cargo-zigbuild"
}

################################################################################
# Quality Gates & Static Cross-Compilation
################################################################################

run_checks() {
    local auto_fmt="${1:-1}"
    log_header "Static Analysis & Syntax Checks"
    (
        cd -- "${PROJECT_DIR}"
        if [[ "${auto_fmt}" -eq 1 ]]; then
            log_info "Formatting Rust sources (cargo fmt)..."
            cargo fmt
        fi
        log_info "Checking Rust formatting (cargo fmt --check)..."
        cargo fmt --check
        log_info "Running Clippy lints (cargo clippy --all-targets --all-features -D warnings)..."
        cargo clippy --all-targets --all-features -- -D warnings
        log_info "Checking Bash syntax (setup.sh, build.sh)..."
        bash -n setup.sh build.sh
    )
    log_ok "All static analysis and syntax checks passed"
}

build_target() {
    local app_name="$1"
    local label="$2"
    local rust_target="$3"
    local bin_path="${PROJECT_DIR}/target/${rust_target}/release/${app_name}"

    log_header "Building ${app_name} [${label} -> ${rust_target}]"
    (cd -- "${PROJECT_DIR}" && cargo zigbuild --release --target "${rust_target}")

    local size_human
    size_human="$(du -h "${bin_path}" | cut -f1)"
    log_ok "Static binary ready: target/${rust_target}/release/${app_name} (${size_human})"
}

################################################################################
# Remote Architecture Detection & Direct 3-File Rsync Deployment
################################################################################

resolve_remote_arch_target() {
    local remote_host="$1"
    local remote_arch
    log_info "Detecting CPU architecture on '${remote_host}' via SSH..."
    remote_arch="$(ssh -- "${remote_host}" uname -m)"

    case "${remote_arch}" in
        x86_64|amd64)
            printf '%s:%s\n' "${LABEL_X86}" "${TARGET_X86}"
            ;;
        aarch64|arm64)
            printf '%s:%s\n' "${LABEL_RPI5}" "${TARGET_RPI5}"
            ;;
        *)
            log_err "Unsupported remote architecture '${remote_arch}' on ${remote_host}"
            exit 1
            ;;
    esac
}

deploy_bundle() {
    local app_name="$1"
    local remote_host="${2:-}"
    local remote_parent="${3:-~}"

    if [[ -z "${remote_host}" ]]; then
        log_err "Usage: ./build.sh deploy <user@host> [remote_parent_dir]"
        exit 2
    fi
    require_cmd "ssh" "Install openssh-clients"
    require_cmd "rsync" "Install rsync"

    local resolved label rust_target bin_src
    resolved="$(resolve_remote_arch_target "${remote_host}")"
    label="${resolved%%:*}"
    rust_target="${resolved#*:}"
    bin_src="${PROJECT_DIR}/target/${rust_target}/release/${app_name}"
    log_ok "Detected '${label}' (${rust_target}) on ${remote_host}"

    verify_build_toolchain
    run_checks
    build_target "${app_name}" "${label}" "${rust_target}"

    log_header "Deploying 3 Files to ${remote_host}:${remote_parent}/${app_name}/"
    rsync -avz --delete \
        "${bin_src}" \
        "${PROJECT_DIR}/setup.sh" \
        "${PROJECT_DIR}/README.md" \
        "${remote_host}:${remote_parent}/${app_name}/"
    log_ok "Deployed ${app_name} (${label}) to ${remote_host}:${remote_parent}/${app_name}/"
}

################################################################################
# CLI Usage & Dispatcher
################################################################################

print_usage() {
    local app_name="$1"
    local app_version="$2"
    cat <<EOF
${app_name} Builder & Deployer v${app_version}

Usage:
  ./build.sh [all]                         Run checks and build static x86_64 & rpi5 binaries
  ./build.sh x86_64                        Run checks and build static PC/Server binary
  ./build.sh rpi5                          Run checks and build static Raspberry Pi 5 binary
  ./build.sh check                         Run cargo fmt, clippy, and bash syntax checks only
  ./build.sh deploy <user@host> [dest_dir] Auto-detect remote arch, build, and rsync the 3 files
  ./build.sh --help                        Show this help message
EOF
}

run_checked_builds() {
    local app_name="$1"
    local mode="$2"
    verify_build_toolchain && run_checks
    case "${mode}" in
        all)
            build_target "${app_name}" "${LABEL_X86}" "${TARGET_X86}"
            build_target "${app_name}" "${LABEL_RPI5}" "${TARGET_RPI5}"
            ;;
        x86_64)
            build_target "${app_name}" "${LABEL_X86}" "${TARGET_X86}"
            ;;
        rpi5)
            build_target "${app_name}" "${LABEL_RPI5}" "${TARGET_RPI5}"
            ;;
    esac
}

dispatch_action() {
    local action="$1"
    local app_name="$2"
    local app_version="$3"
    shift 3 || true

    case "${action}" in
        all)                 run_checked_builds "${app_name}" "all" ;;
        x86|x86_64|amd64)    run_checked_builds "${app_name}" "x86_64" ;;
        rpi|rpi5|arm64|aarch64) run_checked_builds "${app_name}" "rpi5" ;;
        check)               require_cmd "cargo" "Install Rust via https://rustup.rs" && run_checks 0 ;;
        deploy)              deploy_bundle "${app_name}" "${1:-}" "${2:-~}" ;;
        help|--help|-h)      print_usage "${app_name}" "${app_version}" ;;
        *)
            log_err "Unknown action '${action}' (try './build.sh --help')"
            exit 2
            ;;
    esac
}

main() {
    if [[ "${EUID}" -eq 0 && -n "${SUDO_USER:-}" && "${SUDO_USER}" != "root" ]]; then
        log_info "Dropping root privileges to build as '${SUDO_USER}'..."
        exec sudo -u "${SUDO_USER}" -H -- bash "${PROJECT_DIR}/build.sh" "$@"
    fi

    local app_name app_version action
    app_name="$(read_cargo_field "name")"
    app_version="$(read_cargo_field "version")"
    action="${1:-all}"
    shift || true
    dispatch_action "${action}" "${app_name}" "${app_version}" "$@"
}

main "$@"
