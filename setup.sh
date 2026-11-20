#!/bin/bash
# setup.sh - Minimal privilege and bootstrap wrapper for single-binary Rust apps

set -euo pipefail

################################################################################
# Binary Discovery (Repository Checkout & 3-File Release Bundle)
################################################################################

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIR
readonly CARGO_MANIFEST="${SCRIPT_DIR}/Cargo.toml"

read_manifest_name() {
    if [[ ! -f "${CARGO_MANIFEST}" ]]; then
        return 1
    fi
    local parsed
    parsed="$(sed -nE '/^\[package\]/,/^\[/{s/^[[:space:]]*name[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p;}' "${CARGO_MANIFEST}" | head -n 1)"
    [[ -n "${parsed}" ]] || return 1
    printf '%s\n' "${parsed}"
}

find_manifest_binary() {
    local app_name="$1"
    local arch
    arch="$(uname -m)"

    local -a candidates=(
        "${SCRIPT_DIR}/${app_name}"
        "${SCRIPT_DIR}/target/${arch}-unknown-linux-musl/release/${app_name}"
        "${SCRIPT_DIR}/target/release/${app_name}"
        "${SCRIPT_DIR}/target/debug/${app_name}"
    )

    local candidate
    for candidate in "${candidates[@]}"; do
        if [[ -f "${candidate}" && -x "${candidate}" ]]; then
            printf '%s\n' "${candidate}"
            return 0
        fi
    done
    return 1
}

find_bundle_binary() {
    local entry base
    for entry in "${SCRIPT_DIR}"/*; do
        [[ -f "${entry}" && -x "${entry}" && ! -L "${entry}" ]] || continue
        base="$(basename -- "${entry}")"
        case "${base}" in
            *.sh|*.md|*.toml|*.yaml|*.yml|*.json|LICENSE)
                continue
                ;;
            *)
                printf '%s\n' "${entry}"
                return 0
                ;;
        esac
    done
    return 1
}

resolve_binary() {
    local app_name
    if app_name="$(read_manifest_name)"; then
        find_manifest_binary "${app_name}" && return 0
    fi
    find_bundle_binary
}

################################################################################
# Privilege Check & Handoff
################################################################################

requires_root() {
    local arg
    for arg in "$@"; do
        case "${arg}" in
            status|--info|requirements|help|--help|-h|version|--version|-V|-v)
                return 1
                ;;
        esac
    done
    return 0
}

main() {
    local bin_path
    if ! bin_path="$(resolve_binary)"; then
        printf 'Error: compiled Rust binary not found in %s.\n' "${SCRIPT_DIR}" >&2
        printf 'Build it first (e.g., ./build.sh) or run from a release bundle.\n' >&2
        exit 1
    fi

    if requires_root "$@" && [[ "${EUID}" -ne 0 ]]; then
        if ! command -v sudo >/dev/null 2>&1; then
            printf 'Error: root privileges are required, and sudo is not installed.\n' >&2
            exit 1
        fi
        exec sudo -- "${bin_path}" setup "$@"
    fi

    exec "${bin_path}" setup "$@"
}

main "$@"
