# video-tools

`vt` is a TUI and CLI toolbox for video processing.
It shells out to `ffprobe`/`ffmpeg`, which must be on `PATH`.

## Install

```sh
cargo install --path .
```

This installs `vt` to `~/.cargo/bin`.
Rerun it to update.

## Uninstall

```sh
cargo uninstall video-tools
```

## Usage

```sh
vt  # open the TUI in the current directory
```

## Development

```sh
cargo fmt
cargo clippy --all-targets
cargo test
```

Commits and PR titles follow [Conventional Commits](https://www.conventionalcommits.org).
Merges to `main` tag a release, with the version bump derived from those commits.
