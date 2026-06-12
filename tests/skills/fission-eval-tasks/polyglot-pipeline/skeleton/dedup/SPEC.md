# dedup — JSONL merge/dedupe with a conflict policy

A compiled Rust binary (this crate; `serde_json` is already a dependency and
`Cargo.lock` is committed — no other crates are needed).

## CLI

```
dedup [--since YYYY-MM-DD] FILE1.jsonl [FILE2.jsonl ...]   # merged output on stdout
```

- At least one file argument; with no file arguments print usage to stderr
  and exit 2.
- `--since DATE` is optional and, when present, comes **before** the file
  arguments. `DATE` must match `YYYY-MM-DD` (digits in those positions), else
  usage error: exit 2.
- Exit 0 on success. Exit non-zero on an unreadable file or a line that is
  not valid JSON / not a JSON object.
- Skip lines that are empty/whitespace-only.

## Record validation (skip, don't fail)

A line that **is** a JSON object but does not conform to the record schema is
**skipped silently** (it contributes nothing — no position, no tags, no
candidacy). Conforming means all of:

- `id` is a non-empty string;
- `name` is a string;
- `email` is a string or `null`;
- `amount` is a number (JSON `true`/`false` are **not** numbers);
- `date` is a string matching `^[0-9]{4}-[0-9]{2}-[0-9]{2}$`;
- `tags` is an array whose elements are all strings.

Extra keys beyond the six are allowed and preserved on the winner.

## `--since` filter

When `--since DATE` is given, conforming records with `date < DATE` (string
compare) are dropped **before** anything else — they get no position, join no
group, and contribute **no tags**.

## Merge semantics

Think of the surviving records (conforming, and past the `--since` filter)
as one global sequence: files in argument order, lines in file order. Each
surviving record's **position** is its index in that sequence (skipped and
filtered lines do not consume positions).

1. Group records by exact `id` string equality.
2. **Conflict policy — pick a winner per group**, comparing in order:
   (a) newest `date` wins (dates are `YYYY-MM-DD`, so plain string comparison
       orders them);
   (b) if several tie on the newest date, a record whose `email` is non-null
       beats a record whose `email` is null;
   (c) if still tied, the **largest position** (latest in the global
       sequence) wins.
3. The output record for a group is the winner's record **except**:
   - `tags`: the union of the `tags` arrays of *all* records in the group
     (winner and losers alike), deduplicated and sorted ascending (byte
     order);
   - **email backfill:** if the winner's `email` is null and any group member
     has a non-null `email`, the output `email` is the email of the member
     that is newest by `date`, breaking date ties by largest position, among
     the members with non-null `email`. If every member's `email` is null it
     stays `null`.
4. Every other field (`id`, `name`, `amount`, `date`, and any extra keys)
   comes from the winner only.

## Output

One JSON object per line, **groups sorted by `id` ascending** (byte order).
JSON key order within a line does not matter. `amount` must be emitted as a
JSON number equal to the winner's amount.

## Performance budget

A 500,000-line input (across one or more files) must merge in under
**10 seconds** (the grader enforces this on a generated input). Stream the
lines and keep per-group running state in a hash map; storing or re-scanning
the whole input per record will not fit the budget.

## Starter test

```
bash dedup/tests/cli_test.sh     # builds (cargo build --release) and runs the binary
```
