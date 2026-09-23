#!/bin/bash
# cargo-win.sh — bash entry to cargo-win.bat (see that file for the why).
# MSYS2_ARG_CONV_EXCL stops Git Bash from rewriting `/c` into `C:\`.
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
BAT=$(cygpath -w "$SCRIPT_DIR/cargo-win.bat")
MSYS2_ARG_CONV_EXCL='*' cmd.exe /c "$BAT" $*
