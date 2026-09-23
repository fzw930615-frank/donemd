Add-Type @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public class W {
 [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr l);
 public delegate bool EnumWindowsProc(IntPtr h, IntPtr l);
 [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
 [DllImport("user32.dll")] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
 [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
 [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
 public static void Run(uint target) {
   EnumWindows((h,l) => { uint p; GetWindowThreadProcessId(h, out p); if (p==target) { var t=new StringBuilder(256); GetWindowText(h,t,256); var c=new StringBuilder(256); GetClassName(h,c,256); if (t.Length>0) Console.WriteLine((IsWindowVisible(h)?"VIS ":"hid ")+h.ToString()+" ["+c+"] "+t.ToString()); } return true; }, IntPtr.Zero);
 }
}
'@
$procs = Get-Process donemd -ErrorAction SilentlyContinue
foreach ($p in $procs) {
  Write-Output ("PID " + $p.Id)
  [W]::Run([uint32]$p.Id)
}
