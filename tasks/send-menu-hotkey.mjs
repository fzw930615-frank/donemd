// Trigger the AI settings menu accelerator (Ctrl+,) via WScript.SendKeys.
import { execFile } from 'node:child_process';

execFile('powershell.exe',
  ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', 'D:/Coding/donemd/tasks/send-menu-hotkey.ps1'],
  { windowsHide: true },
  (err, stdout, stderr) => {
    console.log('stdout:', (stdout ?? '').trim());
    if (stderr) console.log('stderr:', stderr.trim());
    if (err) console.log('err:', err.message);
  });
