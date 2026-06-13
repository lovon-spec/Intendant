# reference/ — held-back solutions (NOT for agents)

Full, independently-written solutions for each component, used only by the
verifier self-test (README.md "Validation"). The SKILL runner copies *only*
`skeleton/` into the agent's workdir, never this directory. These exist so we
can prove `verify.sh` awards full marks to a correct implementation (and 0 to
the stubs). They are a third implementation, distinct from both the agent's and
the oracle in `verify/oracle.py`; three-way agreement on random inputs is the
correctness argument for the whole verifier.

Assemble a reference workdir for the self-test:

```bash
W=$(mktemp -d)
cp -R skeleton/. "$W"/
cp reference/normalizer/normalize.py   "$W"/normalizer/normalize.py
cp reference/quarantine/quarantine.py  "$W"/quarantine/quarantine.py
cp reference/dedup/src/main.rs         "$W"/dedup/src/main.rs
cp reference/report/report.sh          "$W"/report/report.sh
./verify.sh "$W" --seed 12345   # expect component_scores all 1.0, integration 1.0, total 5.0
```
