# video-tools

[![build](https://github.com/will-molloy/video-tools/actions/workflows/build.yml/badge.svg?branch=main&event=push)](https://github.com/will-molloy/video-tools/actions/workflows/build.yml)
[![codecov](https://codecov.io/gh/will-molloy/video-tools/branch/main/graph/badge.svg)](https://codecov.io/gh/will-molloy/video-tools)

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
vt                  # open the TUI in the current directory
vt chapters FILE    # print a YouTube description chapter list
vt sync-check FILE  # diagnose audio/video sync problems
```
