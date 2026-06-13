#!/usr/bin/env python3
"""Read-only metrics service over the API. See metrics/SPEC.md."""
import argparse
import sys


def main(argv):
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--api", required=True)
    ap.add_argument("--host", default="127.0.0.1")
    args = ap.parse_args(argv[1:])
    # TODO: implement the metrics service per metrics/SPEC.md
    print("metrics/metrics.py: not implemented", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
