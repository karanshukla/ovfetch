# CLAUDE.md

## Temp files and large downloads

`/tmp` is tmpfs on this machine, and so is Claude Code's scratchpad (it lives under `/tmp`), so anything written there is held in RAM. Put venvs, model and runtime downloads, extracted archives and build output on disk under `~/.cache/claude-scratch/<task>/` instead, and keep only small throwaway files in the scratchpad. An in-repo `.venv` or `target/` is already on disk and fine.

## Layout and contract

`docs/development.md` has the CI gate, the module table, and the list of what other vino repos depend on (commands, `detect` JSON, the lock and sums files, `ovfetch-preload.txt` and its line order). Keep that table true when a module moves. The README is an index: long-form material goes in `docs/`.

The version bounds are pessimistic on purpose: "newest recorded as working", never "fails past". Keep the wording and the behaviour.

Comments: one line, only for a constraint the code cannot show. A rule gets a test named after it, measurements go in `docs/` with their date and machine.
