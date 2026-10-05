# Install claude-presence from GitHub Releases (Windows).
#
#   irm https://github.com/vx9k/claude-presence/releases/latest/download/install.ps1 | iex
#
# Environment:
#   CLAUDE_PRESENCE_VERSION      release to install, e.g. 0.2.0 (default: latest)
#   CLAUDE_PRESENCE_NO_SETUP=1   only place the binaries; don't run
#                                `claude-presence install` (hooks, service, PATH)
#   CLAUDE_PRESENCE_INSTALL_DIR  where the binaries go; only with NO_SETUP,
#                                since `claude-presence install` always copies
#                                itself to %LOCALAPPDATA%\Programs\claude-presence
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
# Everything runs inside one script block, so a truncated download executes
# nothing, and errors are thrown rather than `exit`ing the caller's shell.

& {
    Set-StrictMode -Version 3
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue' # Invoke-WebRequest is far slower with the progress bar
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $repo = 'vx9k/claude-presence'
    $defaultDir = Join-Path $env:LOCALAPPDATA 'Programs\claude-presence'
    $noSetup = $env:CLAUDE_PRESENCE_NO_SETUP -eq '1'
    $dir = if ($env:CLAUDE_PRESENCE_INSTALL_DIR) { $env:CLAUDE_PRESENCE_INSTALL_DIR } else { $defaultDir }
    if (-not $noSetup -and $dir -ne $defaultDir) {
        throw "CLAUDE_PRESENCE_INSTALL_DIR needs CLAUDE_PRESENCE_NO_SETUP=1: 'claude-presence install' always installs to $defaultDir"
    }

    function Say([string]$msg) { Write-Host "claude-presence: $msg" }

    function Get-Target {
        # The machine's architecture, not this process's: an x64 PowerShell
        # emulated on ARM64 sees AMD64 in PROCESSOR_ARCHITECTURE (and, on
        # .NET Framework, in RuntimeInformation.OSArchitecture), so ask
        # IsWow64Process2 (Windows 10 1709+) first.
        $arch = $null
        try {
            if (-not ('ClaudePresenceInstall.Native' -as [type])) {
                Add-Type -Namespace ClaudePresenceInstall -Name Native -MemberDefinition @'
[DllImport("kernel32.dll")]
public static extern bool IsWow64Process2(IntPtr process, out ushort processMachine, out ushort nativeMachine);
'@
            }
            $pm = [uint16]0
            $nm = [uint16]0
            $self = [Diagnostics.Process]::GetCurrentProcess().Handle
            if ([ClaudePresenceInstall.Native]::IsWow64Process2($self, [ref]$pm, [ref]$nm)) {
                $arch = switch ($nm) { 0x8664 { 'AMD64' } 0xAA64 { 'ARM64' } default { "machine 0x{0:X4}" -f $nm } }
            }
        } catch { }
        if (-not $arch) {
            $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
        }
        switch -Regex ($arch) {
            '^(X64|AMD64)$' { return 'x86_64-pc-windows-msvc' }
            '^(Arm64|ARM64)$' { return 'aarch64-pc-windows-msvc' }
            default { throw "unsupported architecture: $arch" }
        }
    }

    # Runs a native command, returning its exit code and combined output.
    # Windows PowerShell turns redirected stderr into errors, which 'Stop'
    # would throw.
    function Invoke-Native([string]$exe, [string[]]$argv) {
        $ErrorActionPreference = 'Continue'
        $out = & $exe @argv 2>&1 | ForEach-Object { "$_" }
        [pscustomobject]@{ Code = $LASTEXITCODE; Output = ($out -join "`n") }
    }

    function Get-File([string]$url, [string]$out) {
        try { Invoke-WebRequest -Uri $url -OutFile $out -UseBasicParsing }
        catch { throw "download failed: $url ($($_.Exception.Message))" }
    }

    # A running daemon holds its .exe open; Windows allows renaming it, not
    # overwriting it, so move it aside (as `claude-presence install` does).
    function Copy-Binary([string]$src, [string]$dst) {
        $old = "$dst.old"
        Remove-Item -LiteralPath $old -Force -ErrorAction SilentlyContinue
        try { Copy-Item -LiteralPath $src -Destination $dst -Force }
        catch {
            if (-not (Test-Path -LiteralPath $dst)) { throw }
            Move-Item -LiteralPath $dst -Destination $old -Force
            try { Copy-Item -LiteralPath $src -Destination $dst -Force }
            catch { Move-Item -LiteralPath $old -Destination $dst -Force; throw }
        }
    }

    $target = Get-Target
    $version = "$env:CLAUDE_PRESENCE_VERSION".TrimStart('v')
    $base = if ($version) { "https://github.com/$repo/releases/download/v$version" } else { "https://github.com/$repo/releases/latest/download" }

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("claude-presence-" + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        Say "fetching checksums ($base/SHA256SUMS)"
        Get-File "$base/SHA256SUMS" "$tmp\SHA256SUMS"
        # The archive name carries the version, so "latest" is resolved from
        # the checksum file instead of a GitHub API call.
        $pattern = '^([0-9a-f]{64}) [ *](claude-presence-([0-9A-Za-z.+-]+)-' + [regex]::Escape($target) + '\.zip)$'
        $m = Get-Content -LiteralPath "$tmp\SHA256SUMS" | ForEach-Object { [regex]::Match($_, $pattern) } |
            Where-Object { $_.Success } | Select-Object -First 1
        if (-not $m) { throw "no archive for $target in this release" }
        $archive = $m.Groups[2].Value
        $version = $m.Groups[3].Value
        # Pin the download to the resolved release so a release published in
        # the meantime can't swap it.
        $base = "https://github.com/$repo/releases/download/v$version"

        # Keyless Sigstore signature on SHA256SUMS, from this repo's release
        # workflow for this version's tag. The signature names the tag, so a
        # SHA256SUMS lying about its version fails here; its hash is trusted
        # only after this. A bad signature always aborts; a missing cosign or
        # signature only with CLAUDE_PRESENCE_REQUIRE_SIGNATURE=1.
        $requireSig = $env:CLAUDE_PRESENCE_REQUIRE_SIGNATURE -eq '1'
        $bundle = "$tmp\SHA256SUMS.sigstore.json"
        if (-not (Get-Command cosign -ErrorAction SilentlyContinue)) {
            if ($requireSig) { throw "cosign not found; can't verify the signature (CLAUDE_PRESENCE_REQUIRE_SIGNATURE=1)" }
            Say 'signature not verified (install cosign to check it)'
        } else {
            $haveBundle = $true
            try { Get-File "$base/SHA256SUMS.sigstore.json" $bundle }
            catch {
                if ($requireSig) { throw "can't download SHA256SUMS.sigstore.json (CLAUDE_PRESENCE_REQUIRE_SIGNATURE=1)" }
                Say 'warning: no signature for this release; signature not verified (checksums still checked)'
                $haveBundle = $false
            }
            if ($haveBundle) {
                # Exact identity, not a regexp: the tag is the resolved version,
                # so an older release's genuine SHA256SUMS can't stand in for this one.
                $r = Invoke-Native cosign @('verify-blob', '--bundle', $bundle,
                    '--certificate-identity', "https://github.com/$repo/.github/workflows/release.yml@refs/tags/v$version",
                    '--certificate-oidc-issuer', 'https://token.actions.githubusercontent.com', "$tmp\SHA256SUMS")
                if ($r.Code -ne 0) {
                    Write-Host $r.Output
                    throw 'signature verification failed for SHA256SUMS (if cosign can''t read the bundle, update it)'
                }
                Say 'signature OK'
            }
        }
        $want = $m.Groups[1].Value

        Say "downloading $archive"
        $zip = Join-Path $tmp $archive
        Get-File "$base/$archive" $zip
        $got = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($got -ne $want) { throw "checksum mismatch for $archive (expected $want, got $got)" }
        Say 'checksum OK'

        # Sigstore build provenance: from this repo's release workflow, for
        # this version's tag, on a GitHub-hosted runner.
        if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
            Say 'gh not found; skipping attestation check (checksum verified)'
        } elseif ((Invoke-Native gh @('attestation', 'verify', '--help')).Code -ne 0) {
            # `gh attestation` arrived in gh 2.49.
            Say 'this gh has no attestation command (needs 2.49+); skipping attestation check (checksum verified)'
        } elseif ((Invoke-Native gh @('auth', 'status')).Code -ne 0) {
            Say 'gh is not logged in; skipping attestation check (checksum verified)'
        } else {
            $r = Invoke-Native gh @('attestation', 'verify', $zip, '--repo', $repo,
                '--signer-workflow', "$repo/.github/workflows/release.yml",
                '--source-ref', "refs/tags/v$version", '--deny-self-hosted-runners')
            if ($r.Code -ne 0) {
                Write-Host $r.Output
                throw "attestation verification failed for $archive"
            }
            Say 'attestation OK'
        }

        $x = Join-Path $tmp 'x'
        Expand-Archive -LiteralPath $zip -DestinationPath $x
        $names = 'claude-presence.exe', 'claude-presenced.exe'
        foreach ($n in $names) {
            if (-not (Test-Path -LiteralPath (Join-Path $x $n))) { throw "$archive does not contain $n" }
        }

        if ($noSetup) {
            New-Item -ItemType Directory -Path $dir -Force | Out-Null
            foreach ($n in $names) { Copy-Binary (Join-Path $x $n) (Join-Path $dir $n) }
            Say "installed claude-presence $version to $dir"
            Say "skipping setup; run `"$dir\claude-presence.exe`" install when ready"
            Say 'a daemon that is already running keeps the old version until it restarts'
        } else {
            # `install` run from the extracted copy stops a running daemon,
            # copies both binaries to $defaultDir (moving locked ones aside),
            # wires the hooks to that copy, sets up the service and adds the
            # directory to the user PATH.
            Say "installing claude-presence $version"
            & (Join-Path $x 'claude-presence.exe') install
            if ($LASTEXITCODE -ne 0) { throw "'claude-presence install' failed (exit code $LASTEXITCODE)" }
        }
        # Another copy earlier on PATH (e.g. from `cargo install`) would shadow this one.
        $found = Get-Command claude-presence -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($found -and (Split-Path -Parent $found.Source) -ne $dir) {
            Say "warning: claude-presence on your PATH is $($found.Source), not the one just installed in $dir"
        }
    } finally {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}
