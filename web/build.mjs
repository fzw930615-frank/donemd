// Cross-platform build entry: vite-plugin-singlefile requires one build pass
// per HTML entry (see vite.config.ts). The npm `build` script used to set
// VITE_ENTRY with POSIX inline-env syntax, which cmd.exe can't parse — this
// script keeps `npm run build` working on both macOS and Windows.
import { spawnSync } from 'node:child_process';

const entries = ['visual', 'markdown-source', 'settings', 'outline'];

for (const entry of entries) {
  const result = spawnSync('vite', ['build'], {
    stdio: 'inherit',
    shell: true, // resolve vite.cmd on Windows
    env: { ...process.env, VITE_ENTRY: entry },
  });
  if (result.status !== 0) process.exit(result.status ?? 1);
}
