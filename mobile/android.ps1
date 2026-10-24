# Runs a `cargo tauri android ...` command with the Android environment set for this shell only.
# Usage:  .\android.ps1 build --debug --apk --target aarch64
#         .\android.ps1 dev
$sdk = Join-Path $env:LOCALAPPDATA "Android\Sdk"
$studio = "C:\Program Files\Android\Android Studio"
$ndk = (Get-ChildItem (Join-Path $sdk "ndk") -Directory | Sort-Object Name | Select-Object -Last 1).FullName

$env:JAVA_HOME = Join-Path $studio "jbr"
$env:ANDROID_HOME = $sdk
$env:NDK_HOME = $ndk
$env:Path = "$env:JAVA_HOME\bin;$sdk\platform-tools;$env:Path"

Set-Location (Join-Path $PSScriptRoot "src-tauri")
cargo tauri android @args
