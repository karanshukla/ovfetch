# CLAUDE.md

## Temp files and large downloads

`/tmp` is tmpfs on this machine, and so is Claude Code's scratchpad (it lives under `/tmp`), so anything written there is held in RAM. Put venvs, model and runtime downloads, extracted archives and build output on disk under `~/.cache/claude-scratch/<task>/` instead, and keep only small throwaway files in the scratchpad. An in-repo `.venv` or `target/` is already on disk and fine.
