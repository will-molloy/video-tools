class UsageError(Exception):
    """An expected failure whose message alone tells the user what to do, so no traceback."""


class Terminated(BaseException):
    """Raised by the SIGTERM handler. A `BaseException`, so `except Exception` can't swallow it."""
