param(
    [Parameter(Mandatory = $true)][string]$Target,
    [ValidateSet('stable', 'nightly')][string]$Channel = 'stable'
)
$ErrorActionPreference = 'Stop'

$null = cargo metadata --locked --no-deps --format-version 1
if ($LASTEXITCODE -ne 0) { throw 'Cannot read Cargo metadata' }
# 宿主引擎是库，唯一发布的二进制是沙箱执行体；版本以工作区根为准。
$version = (Get-Content -Raw 'version.txt').Trim()
if (-not $version) { throw 'version.txt is empty' }
$packageVersion = $version
if ($Channel -eq 'nightly') {
    if ($env:GITHUB_SHA -notmatch '^[a-f0-9]{40}$' -or $env:GITHUB_RUN_ID -notmatch '^\d+$') {
        throw 'Nightly packaging requires GITHUB_SHA and GITHUB_RUN_ID'
    }
    $packageVersion = "$version-nightly.$($env:GITHUB_RUN_ID).$($env:GITHUB_SHA.Substring(0, 8))"
}
$name = "oext-$packageVersion-$Target"
$stage = Join-Path 'target/package' $name
New-Item -ItemType Directory -Path $stage -Force | Out-Null
New-Item -ItemType Directory -Path 'dist' -Force | Out-Null
if ($Channel -eq 'nightly') {
    $buildInfo = [ordered]@{
        channel = $Channel
        version = $version
        packageVersion = $packageVersion
        commit = $env:GITHUB_SHA
        runId = $env:GITHUB_RUN_ID
        target = $Target
    } | ConvertTo-Json
    [System.IO.File]::WriteAllText((Join-Path $stage 'build-info.json'), $buildInfo)
}
$suffix = if ($Target -like '*windows*') { '.exe' } else { '' }
foreach ($binary in @('oext-sandbox')) {
    Copy-Item -LiteralPath "target/$Target/release/$binary$suffix" -Destination $stage
}
Copy-Item -LiteralPath 'README.md', 'CHANGELOG.md' -Destination $stage
Copy-Item -LiteralPath 'docs', 'examples' -Destination $stage -Recurse -Force
if ($suffix) {
    $archive = "dist/$name.zip"
    Compress-Archive -Path $stage -DestinationPath $archive -Force
} else {
    $archive = "dist/$name.tar.gz"
    tar -czf $archive -C 'target/package' $name
    if ($LASTEXITCODE -ne 0) { throw 'Archive creation failed' }
}
$hash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
$fileName = Split-Path -Leaf $archive
[System.IO.File]::WriteAllText("$archive.sha256", "$hash  $fileName`n")
