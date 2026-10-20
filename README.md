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

### Interactive (TUI)


```sh
vt
```

Each tool is also a subcommand.

### Tools

| Tool                  | Description                         | Requirements                                                                                                                                                                                                |
|-----------------------|-------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `chapters`            | YouTube description chapter list    | `ffprobe`                                                                                                                                                                                                   |
| `sync-check`          | Diagnose audio/video sync problems  | `ffprobe`                                                                                                                                                                                                   |
| `transcribe`          | Extract dialogue to .srt file       | `ffmpeg`, `ffprobe`, `uv`, AMD (ROCm) or NVIDIA GPU ([Whisper](https://huggingface.co/openai/whisper-large-v3-turbo): 5 GB VRAM)                                                                            |
| `dialogue-separation` | Remove background noise or dialogue | `ffmpeg`, `ffprobe`, `uv`, AMD (ROCm) or NVIDIA GPU ([TIGER](https://huggingface.co/JusperLee/TIGER-DnR): 2 GB VRAM; [SAM](https://huggingface.co/facebook/sam-audio-large): 16 GB and Hugging Face access) |

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).
