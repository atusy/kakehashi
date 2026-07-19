import queue
import os
import pathlib
import threading
import time


def read_with_timeout(stream, operation, timeout_seconds=5):
    result = queue.Queue(maxsize=1)

    def read():
        try:
            result.put((True, operation(stream)))
        except BaseException as error:
            result.put((False, error))

    threading.Thread(target=read, daemon=True).start()
    try:
        succeeded, value = result.get(timeout=timeout_seconds)
    except queue.Empty as error:
        raise TimeoutError("timed out waiting for subprocess output") from error
    if not succeeded:
        raise value
    return value


def readline_with_timeout(stream, timeout_seconds=5):
    return read_with_timeout(stream, lambda source: source.readline(), timeout_seconds)


def read_count_with_timeout(stream, count, timeout_seconds=5):
    return read_with_timeout(stream, lambda source: source.read(count), timeout_seconds)


def process_is_running(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    stat = pathlib.Path(f"/proc/{pid}/stat")
    if stat.is_file():
        try:
            return stat.read_text().split()[2] != "Z"
        except (IndexError, OSError):
            pass
    return True


def wait_for_process_stopped(pid, timeout_seconds=5):
    deadline = time.monotonic() + timeout_seconds
    while process_is_running(pid):
        if time.monotonic() >= deadline:
            raise TimeoutError(f"process {pid} remained running")
        time.sleep(0.01)
