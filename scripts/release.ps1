# Сборка релиза Clipback: установщик, portable-версия и latest.json для обновления из программы.
#
#   .\scripts\release.ps1             — собрать всё в dist\
#   .\scripts\release.ps1 -Publish    — собрать и выложить релиз на GitHub (нужен gh)
#
# Версия берётся из app\tauri.conf.json — перед выпуском поменяйте её там,
# в app\Cargo.toml и core\Cargo.toml.
#
# Обновления подписываются закрытым ключом (по умолчанию %USERPROFILE%\.tauri\clipback.key,
# пароль — в clipback.key.password рядом). Ключ не должен попасть в репозиторий;
# если его потерять, уже установленные копии не смогут обновиться сами.

param(
    [switch]$Publish,
    [string]$Key = "$env:USERPROFILE\.tauri\clipback.key",
    [string]$Repo = "marakaybo/clipback",
    [string]$Notes = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$app = Join-Path $root "app"
$dist = Join-Path $root "dist"

$version = (Get-Content (Join-Path $app "tauri.conf.json") -Raw | ConvertFrom-Json).version
$tag = "v$version"
Write-Host "Clipback $version"

if (-not (Test-Path $Key)) { throw "Нет ключа подписи: $Key (создайте: cargo tauri signer generate -w `"$Key`")" }
$env:TAURI_SIGNING_PRIVATE_KEY = Get-Content $Key -Raw
$pwFile = "$Key.password"
$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = if (Test-Path $pwFile) { (Get-Content $pwFile -Raw).Trim() } else { "" }

# установщик (+ подпись .sig)
Push-Location $app
try {
    cargo tauri build --bundles nsis
    if ($LASTEXITCODE -ne 0) { throw "сборка не удалась" }
} finally { Pop-Location }

$nsisDir = Join-Path $app "target\release\bundle\nsis"
$setupSrc = Join-Path $nsisDir "Clipback_${version}_x64-setup.exe"
if (-not (Test-Path $setupSrc)) { throw "не найден установщик $setupSrc" }

if (Test-Path $dist) { Remove-Item $dist -Recurse -Force }
New-Item -ItemType Directory $dist | Out-Null
$setup = "Clipback-$version-setup.exe"
$portable = "Clipback-$version-portable.exe"
Copy-Item $setupSrc (Join-Path $dist $setup)
Copy-Item "$setupSrc.sig" (Join-Path $dist "$setup.sig")
Copy-Item (Join-Path $app "target\release\clipback.exe") (Join-Path $dist $portable)

# подпись portable-версии
Push-Location $app
try {
    cargo tauri signer sign (Join-Path $dist $portable)
    if ($LASTEXITCODE -ne 0) { throw "не удалось подписать portable-версию" }
} finally { Pop-Location }

# latest.json — его программа читает из последнего релиза
$base = "https://github.com/$Repo/releases/download/$tag"
$manifest = [ordered]@{
    version   = $version
    notes     = $Notes
    pub_date  = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    platforms = [ordered]@{
        "windows-x86_64"          = [ordered]@{ signature = (Get-Content (Join-Path $dist "$setup.sig") -Raw).Trim(); url = "$base/$setup" }
        "windows-x86_64-portable" = [ordered]@{ signature = (Get-Content (Join-Path $dist "$portable.sig") -Raw).Trim(); url = "$base/$portable" }
    }
}
$json = $manifest | ConvertTo-Json -Depth 5
[IO.File]::WriteAllText((Join-Path $dist "latest.json"), $json, (New-Object Text.UTF8Encoding $false))

Write-Host "Готово: $dist"
Get-ChildItem $dist | Format-Table Name, Length

if ($Publish) {
    $files = @($setup, $portable, "latest.json") | ForEach-Object { Join-Path $dist $_ }
    git -C $root tag -a $tag -m "Clipback $version"
    git -C $root push origin $tag
    $notesArgs = if ($Notes) { @("--notes", $Notes) } else { @("--generate-notes") }
    gh release create $tag @files --repo $Repo --title "Clipback $version" @notesArgs
}
