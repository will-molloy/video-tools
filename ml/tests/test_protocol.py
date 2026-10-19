import io

from vt_ml.protocol import Protocol, Stats


def lines(write) -> list[str]:
    out = io.StringIO()
    write(Protocol(out))
    return out.getvalue().splitlines()


def test_writes_progress():
    assert lines(lambda p: p.progress(3, 8)) == ['{"type": "progress", "done": 3, "total": 8}']


def test_writes_log():
    assert lines(lambda p: p.log("Loading model")) == [
        '{"type": "log", "message": "Loading model"}'
    ]


def test_writes_done_with_the_result_and_rounded_stats():
    stats = Stats("cuda:0 (AMD Radeon RX 9070 XT, HIP 7.2)", 12.34567, 9123.456)

    assert lines(lambda p: p.done({"segments": 3}, stats)) == [
        '{"type": "done", "segments": 3, "device": "cuda:0 (AMD Radeon RX 9070 XT, HIP 7.2)", '
        '"elapsed_s": 12.346, "peak_vram_mb": 9123.5}'
    ]


def test_writes_done_on_cpu_without_vram():
    assert lines(lambda p: p.done({}, Stats("cpu", 1.0, None))) == [
        '{"type": "done", "device": "cpu", "elapsed_s": 1.0, "peak_vram_mb": null}'
    ]


def test_writes_error_with_newlines_escaped():
    assert lines(lambda p: p.error("bad\ninput")) == ['{"type": "error", "message": "bad\\ninput"}']
