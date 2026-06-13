#!/usr/bin/env python3
"""Job worker: compute op semantics + a serve loop. See worker/SPEC.md."""
import sys


def main(argv):
    if len(argv) >= 2 and argv[1] == "compute":
        # TODO: implement compute per worker/SPEC.md
        print("worker compute: not implemented", file=sys.stderr)
        return 2
    if len(argv) >= 2 and argv[1] == "serve":
        # TODO: implement the serve loop per worker/SPEC.md
        print("worker serve: not implemented", file=sys.stderr)
        return 2
    print("usage: worker.py compute OP INPUT_JSON | serve API_URL [--once]", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
