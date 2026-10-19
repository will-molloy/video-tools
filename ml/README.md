# vt-ml

The machine-learning stages of `vt`.
`vt` handles probing, extraction and writing outputs; each stage runs one model over audio `vt` prepares.

## Setup

```sh
uv sync --extra <model> --extra rocm  # --extra cuda on NVIDIA
uvx hf auth login                     # for gated models
uv run pytest
```

`vt` runs `uv run --extra <model> --extra <rocm|cuda>`, which installs both extras on first use.
It picks `rocm` or `cuda` from the loaded GPU driver, or from `VT_GPU`.
`fake` backends need no extras.

## Contract

### Invocation

```sh
uv run --project ml [--extra <model> --extra rocm|cuda] vt-ml <stage> [stage arguments]
```

### Stdout

One JSON object per line, flushed per line.
Nothing else is written to stdout; library output is redirected to stderr.

| `type`     | Fields                                                           | When                 |
| ---------- | ---------------------------------------------------------------- | -------------------- |
| `log`      | `message`                                                        | Status, e.g. loading |
| `progress` | `done`, `total`                                                  | As work completes    |
| `done`     | The stage's result fields, `device`, `elapsed_s`, `peak_vram_mb` | Last line on success |
| `error`    | `message`                                                        | Last line on failure |

- `device` describes the device, e.g. `cuda:0 (AMD Radeon RX 9070 XT, HIP 7.2)`, `cuda:0 (NVIDIA GeForce RTX 4090, CUDA 12.8)` or `cpu`
- `peak_vram_mb` is the peak memory torch reserved, or `null` on CPU
- `elapsed_s` covers model loading and the stage's work

`vt` rejects any line that isn't one of these messages.

### Stderr

Human-readable logs and tracebacks. `vt` shows the last lines when a stage fails without an `error` message.

### Exit codes

| Code | Meaning                                     |
| ---- | ------------------------------------------- |
| 0    | Success, after a `done` line                |
| 2    | Error, after an `error` line                |
| 143  | Stopped by SIGTERM, partial outputs removed |

Stages check for SIGTERM between chunks, so stopping can take one chunk's inference.
`vt` signals the whole process group and sends SIGKILL after 30 s.

## GPUs

- The `rocm` and `cuda` extras conflict, and each pulls torch from its own PyTorch index
- `rocm` uses ROCm 7.2, the newest index with a torchaudio build, which pins torch to 2.11
- `cuda` uses CUDA 12.8, which covers NVIDIA's GPUs from Turing (RTX 20) to Blackwell (RTX 50)
- A model never falls back to the CPU; a missing GPU is an error
