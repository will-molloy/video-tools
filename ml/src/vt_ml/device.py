"""Checks the requested torch device exists and describes it. Never falls back to another."""

import torch

from vt_ml.errors import UsageError


def resolve(requested: str) -> tuple[torch.device, str]:
    """Returns the device and a description such as "cuda:0 (AMD Radeon RX 9070 XT, HIP 7.2)".
    torch's `cuda` device covers both ROCm (HIP) and CUDA builds."""
    if requested == "cpu":
        return torch.device("cpu"), "cpu"
    if torch.version.hip is not None:
        runtime = f"HIP {_major_minor(torch.version.hip)}"
    elif torch.version.cuda is not None:
        runtime = f"CUDA {_major_minor(torch.version.cuda)}"
    else:
        raise UsageError(f"torch {torch.__version__} is a CPU-only build")
    if not torch.cuda.is_available():
        raise UsageError(
            f"the GPU is unavailable to torch {torch.__version__} ({runtime}). "
            "Check its driver is installed and loaded."
        )
    device = torch.device("cuda:0")
    return device, f"cuda:0 ({torch.cuda.get_device_name(device)}, {runtime})"


def _major_minor(version: str) -> str:
    return ".".join(version.split(".")[:2])


def peak_vram_mb(device: torch.device) -> float | None:
    """Peak memory torch reserved since the process started, which is what the GPU had to
    provide. None on CPU."""
    if device.type != "cuda":
        return None
    return torch.cuda.max_memory_reserved(device) / 2**20
