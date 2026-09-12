#Requires -RunAsAdministrator
<#
.SYNOPSIS
    Builds flamingo-agent and logger-child, installs them under Program Files and registers
    the FlamingoAgent Windows service (automatic start, LocalSystem).
.PARAMETER Uninstall
    Stop and remove the service and the installed binaries. Logs under
    %ProgramData%\FlamingoAgent are kept.
.EXAMPLE
    powershell -ExecutionPolicy Bypass -File .\install.ps1
.EXAMPLE
    powershell -ExecutionPolicy Bypass -File .\install.ps1 -Uninstall
#>
[CmdletBinding()]
param(
    [switch]$Uninstall
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$Root        = $PSScriptRoot
$AgentDir    = Join-Path $Root 'agent-rust'
$ChildDir    = Join-Path $Root 'logger-child'
$ChildBuild  = Join-Path $ChildDir 'build'
$InstallDir  = Join-Path $env:ProgramFiles 'FlamingoAgent'
$AgentExe    = Join-Path $InstallDir 'flamingo-agent.exe'
$ChildExe    = Join-Path $InstallDir 'logger-child.exe'
$ServiceName = 'FlamingoAgent'
$LogDir      = Join-Path $env:ProgramData 'FlamingoAgent'

function Write-Step([string]$Message) {
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Assert-Tool([string]$Name, [string]$Hint) {
    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
        throw "Required tool '$Name' was not found. $Hint"
    }
}

function Invoke-Checked([string]$Exe, [string[]]$Arguments, [string]$WorkingDirectory) {
    Push-Location $WorkingDirectory
    try {
        & $Exe @Arguments
        if ($LASTEXITCODE -ne 0) {
            throw "'$Exe $($Arguments -join ' ')' failed with exit code $LASTEXITCODE"
        }
    } finally {
        Pop-Location
    }
}

function Test-ServiceExists {
    return $null -ne (Get-Service -Name $ServiceName -ErrorAction SilentlyContinue)
}

function Remove-InstalledService {
    if (-not (Test-ServiceExists)) { return }
    if (Test-Path $AgentExe) {
        Invoke-Checked $AgentExe @('--uninstall') $InstallDir
    } else {
        # Binaries are gone but the registration remains: fall back to sc.exe.
        sc.exe stop $ServiceName | Out-Null
        Start-Sleep -Seconds 2
        sc.exe delete $ServiceName | Out-Null
    }
}

if ($Uninstall) {
    Write-Step "Removing $ServiceName"
    Remove-InstalledService
    if (Test-Path $InstallDir) {
        Remove-Item -Recurse -Force $InstallDir
    }
    Write-Host "Removed. Logs in $LogDir were kept." -ForegroundColor Green
    exit 0
}

Write-Step 'Checking prerequisites'
Assert-Tool 'cargo' 'Install Rust from https://rustup.rs (default MSVC toolchain).'
Assert-Tool 'cmake' 'Install Visual Studio Build Tools 2022 with the "Desktop development with C++" workload; it includes CMake.'

Write-Step 'Building flamingo-agent (cargo build --release)'
Invoke-Checked 'cargo' @('build', '--release') $AgentDir

Write-Step 'Building logger-child (CMake, Release, x64)'
Invoke-Checked 'cmake' @('-S', $ChildDir, '-B', $ChildBuild, '-A', 'x64') $Root
Invoke-Checked 'cmake' @('--build', $ChildBuild, '--config', 'Release') $Root

if (Test-ServiceExists) {
    Write-Step "Removing the existing $ServiceName before reinstalling"
    Remove-InstalledService
}

Write-Step "Installing binaries to $InstallDir"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Copy-Item (Join-Path $AgentDir 'target\release\flamingo-agent.exe') $AgentExe -Force
Copy-Item (Join-Path $ChildBuild 'Release\logger-child.exe') $ChildExe -Force

Write-Step "Registering $ServiceName (automatic start, LocalSystem) and starting it"
Invoke-Checked $AgentExe @('--install') $InstallDir

$service = Get-Service -Name $ServiceName
Write-Host ''
Write-Host "FlamingoAgent: $($service.Status)" -ForegroundColor Green
Write-Host "Agent log:     $LogDir\agent.log"
Write-Host "Child log:     $LogDir\child.log  (readable by Administrators and SYSTEM only)"
