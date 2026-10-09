# wvfb-run

`xvfb-run` for Windows. Runs a program on a hidden Win32 desktop (`CreateDesktop`), so it can't pop up windows or steal focus while you work. Useful for agents running visual tests (games, renderers) in the background.

## Install

```
cargo install --path .
```

## Usage

```
wvfb-run [--timeout SECS] [--] COMMAND [ARGS...]
```

```
wvfb-run --timeout 60 example.exe --screenshot C:\Temp\shot.png
```

- **Output passes through.** stdout and stderr reach the caller, even from windowed apps.
- **wvfb-run doesn't take screenshots.** The program saves its own to a path you pass in, the same as under `xvfb-run`.
- **The timeout defaults to 30 s.** Pass `--timeout` for longer runs.
- **Ending `wvfb-run` ends everything it started,** whether the run finishes, times out or is killed (`taskkill /im wvfb-run.exe /f`). No orphans.

## Hang protection

Nobody can click a dialog on a hidden desktop, so:

- **Dialog boxes:** if a MessageBox, assert or alert dialog appears, wvfb-run prints its text and kills the run (exit 125).
- **Crash and missing-DLL dialogs:** suppressed, so the program exits with an error instead.
- **Freezes:** if a window stays "Not responding" for 60 s while the processes use almost no CPU, the run is treated as deadlocked and killed (exit 124). A window that's busy, using real CPU, is left alone.

## Exit codes

| Code | Meaning |
|---|---|
| child's | Normal exit |
| 124 | Timeout or hang |
| 125 | Dialog appeared |
| 127 | Couldn't launch |

## Notes

- **Rendering:** hardware-accelerated rendering works on the hidden desktop (tested with Vulkan).
- **Window size:** follows your real monitor, so set the render size explicitly if your screenshots must be the same on every machine.
- **Audio** still plays. Disable it in the program if needed.
- **No input simulation.** `SendInput` doesn't reach a hidden desktop. Drive the program from inside (scripts, command-line flags).
