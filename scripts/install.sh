#!/bin/sh
# Install claude-presence from GitHub Releases (Linux, macOS).
#
#   curl -fsSL https://github.com/vx9k/claude-presence/releases/latest/download/install.sh | sh
#
# Environment:
#   CLAUDE_PRESENCE_VERSION      release to install, e.g. 0.2.0 (default: latest)
#   CLAUDE_PRESENCE_INSTALL_DIR  where the binaries go (default: ~/.local/bin)
#   CLAUDE_PRESENCE_NO_SETUP=1   only place the binaries; don't run
#                                `claude-presence install` (hooks + service)
#   CLAUDE_PRESENCE_REQUIRE_SIGNATURE=1
#                                abort unless the signature on SHA256SUMS is
#                                verified (needs cosign on PATH)
#
# Verification, in order:
#   1. With cosign on PATH, SHA256SUMS.sigstore.json must be a valid keyless
#      Sigstore signature of SHA256SUMS made by this repo's release workflow
#      for the release's tag (`cosign verify-blob`); a bad signature aborts.
#      Without cosign (or without a signature, for releases that predate
#      signing) this is skipped with a note, unless
#      CLAUDE_PRESENCE_REQUIRE_SIGNATURE=1, which makes either abort.
#   2. The archive must match its line in SHA256SUMS (always required).
#   3. When the GitHub CLI is installed and logged in, the archive's build
#      provenance attestation must verify too (`gh attestation verify`).
#
# Everything runs inside main() so a truncated download executes nothing.

main() {
    set -eu
    repo=vx9k/claude-presence

    if [ "$(id -u)" -eq 0 ]; then
        err "don't run this as root: claude-presence installs per-user hooks and a user service"
    fi
    [ -n "${HOME:-}" ] || err "HOME is not set"

    target=$(detect_target)
    dir=${CLAUDE_PRESENCE_INSTALL_DIR:-$HOME/.local/bin}
    version=${CLAUDE_PRESENCE_VERSION:-}
    version=${version#v}
    if [ -n "$version" ]; then
        base="https://github.com/$repo/releases/download/v$version"
    else
        base="https://github.com/$repo/releases/latest/download"
    fi

    tmp=$(mktemp -d 2>/dev/null || mktemp -d -t claude-presence)
    trap 'rm -rf "$tmp"' EXIT
    trap 'exit 130' INT TERM

    say "fetching checksums ($base/SHA256SUMS)"
    fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS"
    # The archive name carries the version, so "latest" is resolved from the
    # checksum file instead of a GitHub API call.
    # `<sha256> <space or *><name>`: text or binary mode of sha256sum.
    line=$(grep -E "^[0-9a-f]{64} [ *]claude-presence-[0-9A-Za-z.+-]+-$target\.tar\.gz\$" "$tmp/SHA256SUMS" | head -n 1)
    [ -n "$line" ] || err "no archive for $target in this release"
    archive=${line#* }
    archive=${archive#[ *]}
    version=${archive#claude-presence-}
    version=${version%-"$target".tar.gz}
    # Pin the download to the resolved release so a release published in the
    # meantime can't swap it.
    base="https://github.com/$repo/releases/download/v$version"
    # The signature names the tag, so a SHA256SUMS lying about its version
    # fails here; its hash is trusted only after this.
    verify_signature "$tmp/SHA256SUMS" "$base" "$repo" "$version"
    want=${line%% *}

    say "downloading $archive"
    fetch "$base/$archive" "$tmp/$archive"
    got=$(sha256 "$tmp/$archive")
    [ "$got" = "$want" ] || err "checksum mismatch for $archive (expected $want, got $got)"
    say "checksum OK"
    verify_attestation "$tmp/$archive" "$repo" "$version"

    mkdir -p "$tmp/x"
    tar -xzf "$tmp/$archive" -C "$tmp/x"
    mkdir -p "$dir"
    for b in claude-presence claude-presenced; do
        [ -f "$tmp/x/$b" ] || err "$archive does not contain $b"
        # Copy next to the target, then rename over it: a running daemon keeps
        # its old file, and no one ever sees a half-written binary.
        cp "$tmp/x/$b" "$dir/.$b.new"
        chmod 755 "$dir/.$b.new"
        mv -f "$dir/.$b.new" "$dir/$b"
    done
    say "installed claude-presence $version to $dir"

    case ":${PATH:-}:" in
        *":$dir:"*)
            found=$(command -v claude-presence 2>/dev/null || true)
            if [ -n "$found" ] && [ "$found" != "$dir/claude-presence" ]; then
                say "warning: claude-presence on your PATH is $found, not the one just installed in $dir"
            fi
            ;;
        *) say "warning: $dir is not on your PATH; add it in your shell profile to run claude-presence by name" ;;
    esac

    if [ "${CLAUDE_PRESENCE_NO_SETUP:-}" = 1 ]; then
        say "skipping setup; run \"$dir/claude-presence\" install when ready"
        say "a daemon that is already running keeps the old version until it restarts"
        return 0
    fi
    "$dir/claude-presence" install
}

say() {
    printf 'claude-presence: %s\n' "$*" >&2
}

err() {
    say "error: $*"
    exit 1
}

# The release target triple for this machine.
detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$arch" in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) err "unsupported architecture: $arch" ;;
    esac
    case "$os" in
        Linux) echo "$arch-unknown-linux-musl" ;;
        Darwin)
            # An x86_64 shell under Rosetta on Apple silicon: use the native build.
            if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
                arch=aarch64
            fi
            echo "$arch-apple-darwin"
            ;;
        *) err "unsupported OS: $os (on Windows, use install.ps1)" ;;
    esac
}

fetch() {
    try_fetch "$1" "$2" || err "download failed: $1"
}

# Like fetch, but a failed download returns non-zero instead of aborting.
try_fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl --proto '=https' --tlsv1.2 -fsSL --retry 3 -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        # BusyBox wget has no --https-only; the URLs are https either way.
        https_only=
        if wget --help 2>&1 | grep -q -- --https-only; then https_only=--https-only; fi
        # shellcheck disable=SC2086 # empty must expand to nothing
        wget $https_only -q -O "$2" "$1"
    else
        err "need curl or wget"
    fi
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sum=$(sha256sum "$1")
    elif command -v shasum >/dev/null 2>&1; then
        sum=$(shasum -a 256 "$1")
    else
        err "need sha256sum or shasum to verify the download"
    fi
    echo "${sum%% *}"
}

# Checks the keyless Sigstore signature on SHA256SUMS ($1), fetched from the
# release at $2: it must come from repo $3's release workflow running for tag
# v$4. A bad signature always aborts; a missing cosign or signature only
# with CLAUDE_PRESENCE_REQUIRE_SIGNATURE=1.
verify_signature() {
    require=${CLAUDE_PRESENCE_REQUIRE_SIGNATURE:-}
    bundle="$1.sigstore.json"
    if ! command -v cosign >/dev/null 2>&1; then
        [ "$require" != 1 ] || err "cosign not found; can't verify the signature (CLAUDE_PRESENCE_REQUIRE_SIGNATURE=1)"
        say "signature not verified (install cosign to check it)"
        return 0
    fi
    if ! try_fetch "$2/SHA256SUMS.sigstore.json" "$bundle"; then
        [ "$require" != 1 ] || err "can't download SHA256SUMS.sigstore.json (CLAUDE_PRESENCE_REQUIRE_SIGNATURE=1)"
        say "warning: no signature for this release; signature not verified (checksums still checked)"
        return 0
    fi
    # Exact identity, not a regexp: the tag is the resolved version, so an
    # older release's genuine SHA256SUMS can't stand in for this one.
    if ! out=$(cosign verify-blob --bundle "$bundle" \
        --certificate-identity "https://github.com/$3/.github/workflows/release.yml@refs/tags/v$4" \
        --certificate-oidc-issuer https://token.actions.githubusercontent.com "$1" 2>&1); then
        printf '%s\n' "$out" >&2
        err "signature verification failed for SHA256SUMS (if cosign can't read the bundle, update it)"
    fi
    say "signature OK"
}

# Checks the archive's Sigstore build provenance when `gh` can: it must
# come from this repo's release workflow, for this version's tag, on a
# GitHub-hosted runner. A failed verification aborts the install.
verify_attestation() {
    if ! command -v gh >/dev/null 2>&1; then
        say "gh not found; skipping attestation check (checksum verified)"
        return 0
    fi
    # `gh attestation` arrived in gh 2.49.
    if ! gh attestation verify --help >/dev/null 2>&1; then
        say "this gh has no attestation command (needs 2.49+); skipping attestation check (checksum verified)"
        return 0
    fi
    if ! gh auth status >/dev/null 2>&1; then
        say "gh is not logged in; skipping attestation check (checksum verified)"
        return 0
    fi
    gh attestation verify "$1" --repo "$2" --signer-workflow "$2/.github/workflows/release.yml" \
        --source-ref "refs/tags/v$3" --deny-self-hosted-runners >/dev/null ||
        err "attestation verification failed for $(basename "$1")"
    say "attestation OK"
}

main "$@"
