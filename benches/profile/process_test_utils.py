import queue
import threading


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
