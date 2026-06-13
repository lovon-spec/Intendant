#!/usr/bin/env python3
"""Job CLI client. See cli/SPEC.md for the contract."""
import sys

VERBS = ("submit", "submit-batch", "get", "wait", "requeue")


def main(argv):
    if len(argv) < 2:
        print("usage: client.py %s API_URL ..." % "|".join(VERBS), file=sys.stderr)
        return 2
    verb = argv[1]
    if verb in VERBS:
        # TODO: implement the verbs per cli/SPEC.md
        print("client.py %s: not implemented" % verb, file=sys.stderr)
        return 2
    print("unknown verb: %s" % verb, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
