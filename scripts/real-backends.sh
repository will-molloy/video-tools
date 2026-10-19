#!/usr/bin/env bash
# Runs the real dialogue-separation backends end to end on a synthetic 30 s clip, one at a
# time, printing each run's device, elapsed time, peak VRAM, and separation quality.
#
# Usage: real-backends.sh          run SAM Audio and TIGER-DnR
#        real-backends.sh sam      run one backend
#
# SAM Audio needs access to facebook/sam-audio-large on Hugging Face and `uvx hf auth login`.

set -euo pipefail
cd "$(dirname "$0")/.."

case "${1:-}" in
    "" | sam | tiger) ;;
    *)
        echo "usage: $0 [sam|tiger]" >&2
        exit 2
        ;;
esac

cargo test --test dialogue_separation "${1:+$1_}separates_the_synthetic_clip" -- \
    --ignored --nocapture --test-threads=1
