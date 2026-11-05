#!/usr/bin/env python3
"""Bound a qualification command and its children on macOS and Linux."""
import os
import signal
import subprocess
import sys


def main():
    seconds = float(sys.argv[1])
    command = sys.argv[2:]
    if seconds <= 0:
        print("TIMEOUT: qualification stage deadline exhausted", file=sys.stderr)
        return 124
    process = subprocess.Popen(command, start_new_session=True)

    def stop():
        # The leader may have exited while children still hold pipes open.
        for sig in (signal.SIGTERM, signal.SIGKILL):
            try:
                os.killpg(process.pid, sig)
            except ProcessLookupError:
                pass
        process.wait()

    def interrupted(signum, _frame):
        stop()
        sys.exit(128 + signum)

    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    try:
        status = process.wait(timeout=seconds)
        stop()
        return status
    except subprocess.TimeoutExpired:
        print("TIMEOUT: qualification command exceeded deadline: " + command[0], file=sys.stderr)
        stop()
        return 124


if __name__ == "__main__":
    sys.exit(main())
