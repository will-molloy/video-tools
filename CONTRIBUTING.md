# Contributing

Guidelines for both humans and coding agents.

## Product

- Keep it simple. Prefer one obvious behaviour over options, modes and heuristics.
  - For example, dialogue separation processes the whole file instead of sampling it.
  - Add an option only when the simple behaviour has failed on a real file.
- Regarding the ML/AI tools; I wanted to test if I can vibe code DaVinci Resolve Studio's AI features
  (spoiler, I will probably end up buying it).

## Tools

- Each tool has one name and one description, used everywhere.
  - `Tool::name` and `Tool::summary` in `src/tools/mod.rs` hold them.
  - The TUI menu and the CLI help read them from there.
  - The README's tool list repeats them word for word.
- Input files are read-only.
  A tool never writes to, renames, moves or deletes one, even when forced or on failure.
  It writes each result as a new file next to the input, such as `talk.srt` or `talk.voice.mp4` for `talk.mp4`.
- A tool never replaces an existing output without consent.
  It checks with `tools::check_output` before any slow work and fails with `tools::Exists`.
  - The CLI takes `--force` to replace the output, and maps the error with `tools::suggest_force`.
  - The TUI asks to confirm, then reruns the tool forced.
