# reference/ — held-back solutions (NOT for agents)

Full, independently-written solutions for each component, used only by the
verifier self-test. The SKILL runner copies *only* `skeleton/` into the agent's
workdir, never this directory. They prove `verify.sh` awards full marks to a
correct implementation (and 0 to the stubs), and they are a third
implementation distinct from the agent's and the oracle in `verify/`.

Assemble a reference workdir for the self-test:

```bash
W=$(mktemp -d)
cp -R skeleton/. "$W"/
cp reference/api/server.py       "$W"/api/server.py
cp reference/worker/worker.py    "$W"/worker/worker.py
cp reference/cli/client.py       "$W"/cli/client.py
cp reference/metrics/metrics.py  "$W"/metrics/metrics.py
./verify.sh "$W" --seed 12345   # expect all component_scores 1.0, integration 1.0, total 5.0
```
