$ErrorActionPreference = "Stop"

# Build first with the real USERPROFILE (cargo needs its registry), then
# exercise the binary against a synthetic home: ccs must list the fake
# session and exit 0; with no ~/.claude/projects it must exit 1.
cargo build --quiet
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$Bin = "target\debug\ccs.exe"

$TempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("ccs-smoke-" + [guid]::NewGuid().ToString("N"))
$EmptyHome = Join-Path $TempRoot "empty"
$FakeHome = Join-Path $TempRoot "home"
$Proj = Join-Path $FakeHome ".claude\projects\D--ccs-smoke"
New-Item -ItemType Directory -Force -Path $EmptyHome | Out-Null
New-Item -ItemType Directory -Force -Path $Proj | Out-Null

$OldUserPROFILE = $env:USERPROFILE
$OldHOME = $env:HOME

try {
    # 1) no projects directory -> exit 1
    $env:USERPROFILE = $EmptyHome
    $env:HOME = $EmptyHome
    $null = & $Bin --no-tui
    if ($LASTEXITCODE -ne 1) {
        throw "expected exit 1 for missing projects dir, got $LASTEXITCODE"
    }

    # 2) synthetic session -> listed with its topic, exit 0. WriteAllLines
    # would emit CRLF and head_info would miss every record — Claude Code
    # transcripts are LF, so the synthetic file must match (WriteAllText
    # defaults to UTF-8 without BOM).
    $SessionId = "00000000-0000-4000-8000-000000000001"
    $Content = '{"cwd":"D:/ccs-smoke","sessionId":"' + $SessionId + '"}' + "`n" +
        '{"message":{"role":"user","content":"ccs smoke topic zqxwvb"}}' + "`n"
    [System.IO.File]::WriteAllText((Join-Path $Proj ($SessionId + ".jsonl")), $Content)

    $env:USERPROFILE = $FakeHome
    $env:HOME = $FakeHome
    $Out = & $Bin --no-tui -n 5
    if ($LASTEXITCODE -ne 0) {
        throw "expected exit 0 for the synthetic listing, got $LASTEXITCODE"
    }
    $Text = if ($Out -is [array]) { $Out -join "`n" } else { "$Out" }
    if ($Text -notmatch "zqxwvb") {
        throw "listing did not include the fake topic: $Text"
    }
    if ($Text -notmatch "ccs-smoke") {
        throw "listing did not include the fake cwd: $Text"
    }

    Write-Host "ccs platform smoke passed"
}
finally {
    $env:USERPROFILE = $OldUserPROFILE
    $env:HOME = $OldHOME
    Remove-Item -Recurse -Force $TempRoot -ErrorAction SilentlyContinue
}
