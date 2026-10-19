from dataclasses import dataclass


@dataclass(frozen=True)
class Segment:
    """A stretch of speech, in seconds from the start of the input."""

    start: float
    end: float
    text: str
