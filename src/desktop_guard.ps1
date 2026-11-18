$ErrorActionPreference = 'Stop'
$apps = @(Get-Process | Where-Object { $_.ProcessName -in @('Codex', 'ChatGPT') })
$gui = @($apps | Where-Object {
    $path = $_.Path
    if (-not $path) { throw 'Cannot inspect a Codex/ChatGPT process' }
    $directory = [IO.Path]::GetDirectoryName($path)
    # MainWindowHandle can be zero from a sandbox, or when minimized to tray.
    # The bundled CLI lives in resources; the desktop executable lives above it.
    if ($directory -match '[\\/]resources$') { $false }
    else {
        ($_.MainWindowHandle -ne 0) -or
        ($path -match '[\\/]OpenAI\.Codex_[^\\/]+[\\/]app[\\/]') -or
        (Test-Path -LiteralPath (Join-Path $directory 'resources\codex.exe')) -or
        (Test-Path -LiteralPath (Join-Path $directory 'app\resources\codex.exe'))
    }
})
if ($gui.Count -gt 0) { 'running' } else { 'closed' }
