[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)
$ErrorActionPreference = 'SilentlyContinue'
$locations = @(Get-AppxPackage -Name OpenAI.Codex | ForEach-Object { $_.InstallLocation })
$locations += @(Get-AppxPackage -Name OpenAI.ChatGPT* | ForEach-Object { $_.InstallLocation })
$paths = @(Get-Process -Name codex -ErrorAction SilentlyContinue | ForEach-Object {
    try { if ($_.Path) { $_.Path } } catch {}
})
@{ installs = @($locations); processes = @($paths) } | ConvertTo-Json -Compress
