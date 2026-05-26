$Action = New-ScheduledTaskAction -Execute "$env:USERPROFILE\.cargo\bin\local-focus.exe" -Argument "serve"
$Trigger = New-ScheduledTaskTrigger -AtLogOn
$Settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
Register-ScheduledTask -TaskName "Local Focus" -Action $Action -Trigger $Trigger -Settings $Settings -Description "Local Focus activity tracker" -Force
Start-ScheduledTask -TaskName "Local Focus"
Write-Host "Local Focus will start at login. Dashboard: http://127.0.0.1:4799"
