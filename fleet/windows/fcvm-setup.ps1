# Runs once as SYSTEM at the end of Windows Setup (SetupComplete.cmd). Configures the image
# for a disposable sandbox desktop reached over RDP, schedules one restart so the feature
# removal finishes, and powers off at the end of the next boot.
$ErrorActionPreference = 'Continue'
& {
function Step($m) { Write-Output ("{0:o} {1}" -f (Get-Date), $m) }
$ts = 'HKLM:\SYSTEM\CurrentControlSet\Control\Terminal Server'
$rdp = "$ts\WinStations\RDP-Tcp"
$pol = 'HKLM:\SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services'
New-Item -Force $pol | Out-Null

Step 'RDP on, NLA off (the port is reachable only from the host), 60 fps, AVC preferred'
Set-ItemProperty $ts fDenyTSConnections 0
Set-ItemProperty $rdp UserAuthentication 0
Set-ItemProperty $rdp fPromptForPassword 0
New-ItemProperty -Force $ts DWMFRAMEINTERVAL -PropertyType DWord -Value 15 | Out-Null
New-ItemProperty -Force "$ts\WinStations" DWMFRAMEINTERVAL -PropertyType DWord -Value 15 | Out-Null
New-ItemProperty -Force $pol AVC444ModePreferred -PropertyType DWord -Value 1 | Out-Null
New-ItemProperty -Force $pol fEnableWddmDriver -PropertyType DWord -Value 1 | Out-Null
New-ItemProperty -Force $pol MaxCompressionLevel -PropertyType DWord -Value 0 | Out-Null
New-ItemProperty -Force $pol fDisableAutoReconnect -PropertyType DWord -Value 0 | Out-Null
New-ItemProperty -Force $pol MaxDisconnectionTime -PropertyType DWord -Value 0 | Out-Null
New-ItemProperty -Force $pol MaxIdleTime -PropertyType DWord -Value 0 | Out-Null
Enable-NetFirewallRule -DisplayGroup 'Remote Desktop'

Step 'no Server Manager, no shutdown tracker, no Ctrl+Alt+Del, no lock'
New-Item -Force 'HKLM:\SOFTWARE\Microsoft\ServerManager' | Out-Null
Set-ItemProperty 'HKLM:\SOFTWARE\Microsoft\ServerManager' DoNotOpenServerManagerAtLogon 1
Get-ScheduledTask -TaskName ServerManager -ErrorAction SilentlyContinue | Disable-ScheduledTask | Out-Null
New-Item -Force 'HKLM:\SOFTWARE\Policies\Microsoft\Windows NT\Reliability' | Out-Null
Set-ItemProperty 'HKLM:\SOFTWARE\Policies\Microsoft\Windows NT\Reliability' ShutdownReasonOn 0
Set-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System' DisableCAD 1
New-Item -Force 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\Personalization' | Out-Null
Set-ItemProperty 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\Personalization' NoLockScreen 1
New-Item -Force 'HKLM:\SOFTWARE\Policies\Microsoft\WindowsAdminCenter' | Out-Null
New-Item -Force 'HKLM:\SOFTWARE\Microsoft\ServerManager' | Out-Null
Set-ItemProperty 'HKLM:\SOFTWARE\Microsoft\ServerManager' DoNotPopWACConsoleAtSMLaunch 1

Step 'power: never sleep or hibernate'
powercfg /setactive 8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c
powercfg /hibernate off
powercfg /change monitor-timeout-ac 0
powercfg /change standby-timeout-ac 0

Step 'no Windows Update, no Defender, no telemetry upload'
foreach ($s in 'wuauserv','UsoSvc','WaaSMedicSvc','DiagTrack','dmwappushservice','MapsBroker') {
  Stop-Service $s -Force -ErrorAction SilentlyContinue
  Set-Service $s -StartupType Disabled -ErrorAction SilentlyContinue
}
New-Item -Force 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU' | Out-Null
Set-ItemProperty 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU' NoAutoUpdate 1
Set-MpPreference -DisableRealtimeMonitoring $true -ErrorAction SilentlyContinue
Uninstall-WindowsFeature -Name Windows-Defender -ErrorAction SilentlyContinue | Out-String | Write-Output

Step 'no maintenance work in a sandbox: Automatic Maintenance and its tasks off'
# A restored sandbox's clock jumps from the snapshot's time to the host's, and Task Scheduler
# then starts every task whose time has passed. The .NET native image service (mscorsvw,
# queued by the .NET Framework NGEN tasks) took 1.1 cores of a 2-vCPU clone for minutes
# after every restore (measured 2026-11-07 in Task Manager over RDP). Its queue is drained at
# the next start, below, and the tasks that would refill or rerun it are disabled.
New-Item -Force 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Schedule\Maintenance' | Out-Null
Set-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Schedule\Maintenance' MaintenanceDisabled 1
foreach ($p in '\Microsoft\Windows\.NET Framework\', '\Microsoft\Windows\Application Experience\',
               '\Microsoft\Windows\Customer Experience Improvement Program\', '\Microsoft\Windows\Defrag\',
               '\Microsoft\Windows\DiskCleanup\', '\Microsoft\Windows\Servicing\', '\Microsoft\Windows\Maintenance\',
               '\Microsoft\Windows\Windows Error Reporting\', '\Microsoft\Windows\UpdateOrchestrator\',
               '\Microsoft\Windows\WindowsUpdate\', '\Microsoft\Windows\Diagnosis\', '\Microsoft\Windows\DiskDiagnostic\',
               '\Microsoft\Windows\Power Efficiency Diagnostics\', '\Microsoft\Windows\Feedback\Siuf\',
               '\Microsoft\Windows\Autochk\', '\Microsoft\Windows\Chkdsk\', '\Microsoft\Windows\Data Integrity Scan\',
               '\Microsoft\Windows\Registry\', '\Microsoft\Windows\SoftwareProtectionPlatform\') {
  Get-ScheduledTask -TaskPath $p -ErrorAction SilentlyContinue | Disable-ScheduledTask -ErrorAction SilentlyContinue | Out-Null
}
Get-ScheduledTask | Where-Object { $_.TaskPath -like '\Microsoft\Windows\*' -and $_.State -ne 'Disabled' -and
  ($_.TaskPath -like '*.NET Framework*' -or $_.TaskPath -like '*Servicing*') } | Format-Table TaskPath, TaskName, State

Step 'serial console: EMS/SAC on COM1 for boot logs'
bcdedit /emssettings EMSPORT:1 EMSBAUDRATE:115200
bcdedit /ems '{current}' on
bcdedit /bootems '{bootmgr}' on
bcdedit /timeout 0

Step 'activation (evaluation: online within 10 days of installation)'
cscript //nologo C:\Windows\System32\slmgr.vbs /ato
cscript //nologo C:\Windows\System32\slmgr.vbs /dli
cscript //nologo C:\Windows\System32\slmgr.vbs /xpr

Step 'the guest agent: network and clock, at every start'
$a = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument '-NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File C:\fcvm\fcvm-agent.ps1'
$t = New-ScheduledTaskTrigger -AtStartup
$s = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -Priority 0
Register-ScheduledTask -Force -TaskName fcvm-agent -Action $a -Trigger $t -Settings $s -User SYSTEM -RunLevel Highest | Out-Null

Step 'at the next start: activate again if needed, close outbound traffic, power off'
@'
& {
Start-Sleep 30
# Compile the .NET native images now, while the image is being built, so no sandbox does.
foreach ($fw in 'Framework64', 'Framework') {
  $ngen = "$env:windir\Microsoft.NET\$fw\v4.0.30319\ngen.exe"
  $t = Measure-Command { & $ngen executeQueuedItems /silent | Out-Null }
  "ngen $fw executeQueuedItems: exit $LASTEXITCODE in $([int]$t.TotalSeconds) s"
}
cscript //nologo C:\Windows\System32\slmgr.vbs /ato
cscript //nologo C:\Windows\System32\slmgr.vbs /dli
cscript //nologo C:\Windows\System32\slmgr.vbs /xpr
Get-WindowsFeature Windows-Defender | Format-List Name, InstallState
Set-NetFirewallProfile -Profile Domain,Public,Private -Enabled True -DefaultInboundAction Block -DefaultOutboundAction Block
# An enabled allow rule overrides the default block: keep only ours.
Get-NetFirewallRule -Direction Outbound -Action Allow -Enabled True | Disable-NetFirewallRule
New-NetFirewallRule -DisplayName 'fcvm namespace' -Direction Outbound -RemoteAddress 10.0.2.1 -Action Allow | Out-Null
Get-NetFirewallProfile | Format-Table Name, Enabled, DefaultInboundAction, DefaultOutboundAction
Unregister-ScheduledTask -TaskName fcvm-finish -Confirm:$false
Unregister-ScheduledTask -TaskName fcvm-restart -Confirm:$false
Get-Date -Format o
} *>&1 | Out-File -Encoding utf8 -Append C:\fcvm\setup.log
# The image build reads this from COM2 (COM1 is EMS's).
$p = New-Object System.IO.Ports.SerialPort COM2, 115200
$p.Open()
$p.Write("===FCVM-SETUP-LOG===`n" + (Get-Content -Raw -Encoding utf8 C:\fcvm\setup.log) + "`n===FCVM-SETUP-END===`n")
$p.Close()
Stop-Computer -Force
'@ | Set-Content C:\fcvm\finish.ps1
$a = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument '-NoProfile -ExecutionPolicy Bypass -File C:\fcvm\finish.ps1'
Register-ScheduledTask -Force -TaskName fcvm-finish -Action $a -Trigger (New-ScheduledTaskTrigger -AtStartup) -User SYSTEM -RunLevel Highest | Out-Null
# Setup has not finished while SetupComplete.cmd runs, and must not be rebooted from it: a
# task two minutes later restarts the machine so the feature removal completes.
$a = New-ScheduledTaskAction -Execute 'shutdown.exe' -Argument '/r /t 0 /f'
Register-ScheduledTask -Force -TaskName fcvm-restart -Action $a -Trigger (New-ScheduledTaskTrigger -Once -At (Get-Date).AddMinutes(2)) -User SYSTEM -RunLevel Highest | Out-Null
Step 'restart scheduled'
} *>&1 | Out-File -Encoding utf8 -Append C:\fcvm\setup.log
