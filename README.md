# Project Zomboid RAM Configurator

[繁體中文說明](README_zh-TW.md)

A Windows tool for Project Zomboid players to adjust the **client's Java heap limit** (`-Xmx`), including memory-hungry mod lists and modded servers.

## Download and run

1. Download [English: PZ_Memory_Configurator_EN_US.bat](https://github.com/Minidoracat/project-zomboid-ram-configurator/releases/latest/download/PZ_Memory_Configurator_EN_US.bat), or [Traditional Chinese: PZ_Memory_Configurator_ZH_TW.bat](https://github.com/Minidoracat/project-zomboid-ram-configurator/releases/latest/download/PZ_Memory_Configurator_ZH_TW.bat). These links download the latest stable release directly; do not save a GitHub HTML page.
2. **Only one `.bat` in your preferred language is needed. No accompanying `.ps1` is required.** If using Code → Download ZIP, extract it first and take the `.bat` you want.
3. Close the game and wait for Steam updates or verification to finish.
4. Double-click `PZ_Memory_Configurator_EN_US.bat`. The Traditional Chinese launcher is `PZ_Memory_Configurator_ZH_TW.bat`.

No Python, Java installation or additional packages are needed. The tool uses Windows PowerShell 5.1, not WMIC. The target is 64-bit Windows 10/11; this revision was verified on Windows 11. Windows 7, Linux and macOS are not supported.

The launchers use `-NoProfile`, so your personal PowerShell startup scripts are not loaded. `-ExecutionPolicy Bypass` applies only to that PowerShell process: it does not change your permanent execution policy or override organizational policy. Only run downloads from sources you trust.

Current version: **v2.0.0**. See [Releases](https://github.com/Minidoracat/project-zomboid-ram-configurator/releases) for release history and standalone downloads. The repository has been renamed; the familiar `.bat` filenames are unchanged.

## Using the tool

The tool searches Steam and its libraries, then displays the game path, installed RAM, currently free RAM, current heap limit and recommendation.

- Select a target if multiple installations are found.
- If discovery fails, enter or drag in the game folder. It must contain `ProjectZomboid64.json` or its `.bak` backup. A backup-only folder can still enter the restore workflow.
- Spaces, Chinese characters, apostrophes, brackets and exclamation marks in paths are supported.

| Option | Action |
| --- | --- |
| 1 | Apply the heavy-mod recommendation |
| 2 | Enter a whole-number GB limit, including a server's **client-side** recommendation; enter 0 to go back |
| 3 | Restore the complete configuration from `ProjectZomboid64.json.bak` |
| 0 | Quit |

Applying or restoring requires confirmation. Only `y` proceeds; Enter or another response cancels. Changes take effect on the next game launch. The tool never lowers your current setting automatically.

## Heavy-mod recommendation

The recommendation uses the **sum of installed memory-module capacities**, not the amount left free by background applications:

`floor(min(physical RAM × 80%, physical RAM − 4 GB))`

| Installed RAM | Recommended limit |
| --- | --- |
| 8 GB | 4 GB |
| 12 GB | 8 GB |
| 16 GB | 12 GB |
| 24 GB | 19 GB |
| 32 GB | 25 GB |
| 64 GB | 51 GB |

Here, GB means 1024³ bytes, matching the JVM's `g` unit.

- This is a **starting policy for heavy mods**, not an official PZ optimum or a benchmark result.
- `-Xmx` caps the Java heap. It does not allocate the entire amount immediately and is not a cap on total process memory. Increasing it does not guarantee more FPS.
- Below 8 GB, automatic recommendations are disabled but custom input remains available. If installed RAM cannot be detected, only restore/quit are available.
- Custom values must be positive whole numbers **below installed RAM**. They may exceed the recommendation.
- Values above the recommendation, below the current limit, or close to/above the free RAM measured during startup trigger warnings before confirmation. Leaving 4 GB is not a guarantee that every background workload will fit.
- This changes the player's client configuration, not dedicated-server startup parameters. Do not copy a server host's RAM allocation without checking that it is intended for clients.

## Backups and safe writes

- Only the unique `-Xmx` value is replaced. Other content, formatting, Unicode and UTF-8 BOM presence are preserved. `-Xms` is never changed.
- Existing sizes may use `k`, `m`, `g` (either case), or bytes without a suffix. Missing/duplicate/invalid arguments, non-UTF-8 files and ambiguous edits are rejected rather than guessed.
- A new limit below an existing `-Xms` is rejected; resolve that conflict yourself first.
- A temporary file in the same directory is validated before atomic replacement. The complete pre-change file becomes `.bak`. Each actual change replaces that backup with the previous version; setting the same limit writes neither file.
- Read-only files, insufficient permissions or filesystems without atomic replacement cause failure. The tool does not fall back to unsafe direct overwrites. Follow the error details to resolve the cause.
- Some Windows replacement errors can occur after files have already been moved; not every operating-system failure leaves every file unchanged. On replacement failure, the tool retains the candidate and previous-backup copy and reports recovery paths. **Preserve and inspect these files before retrying; do not delete them blindly.**
- If another program changes the configuration while you are confirming, the write is rejected. Read the new state and confirm again.
- The file is read back and checked before success is reported.
- Restore keeps `.bak` intact and can recover a damaged or missing JSON file from a valid backup. **It restores the whole configuration and can undo game updates or manual edits. Do not blindly restore a backup from another game version.**

Steam updates or game verification may reset the limit; open the tool again to check. If the configuration is damaged and no valid backup is available, use Steam's **Verify integrity of game files**, then configure it again.

## Development and verification

Maintain the shared logic only in `PZ_Memory_Configurator.ps1`. `Build-Standalone.ps1` embeds that same core into both root-level `.bat` files. Do not edit the generated `.bat` files directly. They contain readable source and execute the core in memory, without downloading code or creating a temporary `.ps1`.

After editing the core, regenerate the standalone files locally (this does not upload anything or create a Release):

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\Build-Standalone.ps1
```

Use `-OutputDirectory "D:\PZ-release"` for another output folder. Publish the two generated `.bat` assets; players only need one.

Run the isolated regression checks; they do not modify real game settings:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\tests\MemoryConfigurator.Tests.ps1
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\tests\Standalone.Tests.ps1
```

The standalone checks verify that root-level `.bat` files match the current core, packaging is reproducible, and both languages start and immediately quit from an isolated folder with no `.ps1`. They also check nonzero exit codes for truncated downloads. These checks may read local Steam/game information but never modify game settings.

To open the interactive tool for a specific directory:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\PZ_Memory_Configurator.ps1 -Language en-US -GameDirectory "D:\SteamLibrary\steamapps\common\ProjectZomboid"
```

This is the real configuration workflow and still asks for confirmation before writing. Passing regression checks does not establish compatibility with every PZ version, launcher or mod list, or demonstrate a performance improvement.

A real file-lock failure was also exercised to check recovery-file retention. Windows partial-replacement errors `1176`/`1177` and power-loss scenarios were not reproduced.

## Sharing and support

You are welcome to share this tool; please credit the source. Report problems in a GitHub issue or on [Discord](https://discord.gg/Gur2V67). Include the error, Windows version, game version and launch method. Redact personal information from paths before sharing them.
