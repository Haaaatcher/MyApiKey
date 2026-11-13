# Remove orphan files from the Windows target's source tree (files that
# were deleted on the Mac side but linger here). Invoked by
# scripts/sync-windows.sh right after extraction: reads manifest.txt at
# the repo root (the source file list generated with the same exclusion
# rules as the tarball) and deletes every file not listed and outside
# build-artifact directories -- orphan cleanup is automated, no manual
# memory required (2026-11-13 review decision).
#
# Exclusion mirrors sync-windows.sh tar rules: target, node_modules,
# dist and gen directories are skipped entirely (target is the target's
# incremental build cache; deleting it costs a full 20-minute rebuild).
#
# NOTE: kept ASCII-only on purpose. Windows PowerShell 5.1 reads BOM-less
# files as ANSI/GBK, and CJK comments garble the parse into syntax errors.

$root = 'C:\Users\luohang\Projects\MyApiKey'
$manifestPath = Join-Path $root 'manifest.txt'
if (-not (Test-Path $manifestPath)) {
    Write-Output 'SKIP: manifest.txt missing (state before first sync); orphan cleanup skipped'
    exit 0
}

$set = [System.Collections.Generic.HashSet[string]]::new(
    [string[]](Get-Content $manifestPath)
)

$orphans = @()
Get-ChildItem $root -Recurse -File -Force | ForEach-Object {
    $rel = $_.FullName.Substring($root.Length + 1).Replace('\', '/')
    if ($rel -match '(^|/)(target|node_modules|dist|gen)/') { return }
    if ($rel -eq 'manifest.txt') { return }  # the manifest itself, removed at the end
    if ($set.Contains($rel)) { return }
    $orphans += $_
}

foreach ($f in $orphans) {
    Remove-Item -LiteralPath $f.FullName -Force
}
Write-Output "orphan cleanup done: removed $($orphans.Count), manifest entries $($set.Count)"
Remove-Item $manifestPath -Force -ErrorAction SilentlyContinue
