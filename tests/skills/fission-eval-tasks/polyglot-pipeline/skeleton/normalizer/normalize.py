#!/usr/bin/env python3
"""CSV -> JSONL normalizer. See SPEC.md for the contract."""
import sys


def main(argv):
    if len(argv) != 3:
        print("usage: normalize.py INPUT.csv OUTPUT.jsonl", file=sys.stderr)
        return 2
    # TODO: implement per normalizer/SPEC.md
    print("normalize.py: not implemented", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
