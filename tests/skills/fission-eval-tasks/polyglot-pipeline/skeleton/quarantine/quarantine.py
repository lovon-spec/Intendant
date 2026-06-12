#!/usr/bin/env python3
"""Business-rule screen: clean vs quarantined. See SPEC.md for the contract."""
import sys


def main(argv):
    # Expected usage: quarantine.py --as-of YYYY-MM-DD INPUT.jsonl CLEAN.jsonl QUAR.jsonl
    if len(argv) != 6 or argv[1] != "--as-of":
        print("usage: quarantine.py --as-of YYYY-MM-DD INPUT.jsonl CLEAN.jsonl QUAR.jsonl",
              file=sys.stderr)
        return 2
    # TODO: implement per quarantine/SPEC.md
    print("quarantine.py: not implemented", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
