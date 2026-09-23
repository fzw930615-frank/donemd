$ws = New-Object -ComObject WScript.Shell
$ok = $ws.AppActivate("Done.md")
Write-Output ("activated: " + $ok)
Start-Sleep -Milliseconds 400
# Ctrl+, is the AI settings menu accelerator (menu.rs)
$ws.SendKeys("^,")
Write-Output "sent ctrl+comma"
Start-Sleep -Seconds 3
Write-Output "waited"
