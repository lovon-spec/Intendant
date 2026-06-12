# normalizer — CSV → JSONL

## CLI

```
python3 normalizer/normalize.py INPUT.csv OUTPUT.jsonl
```

- Exit 0 on success — including when every row was rejected (an empty output
  file is valid output).
- Exit non-zero for usage errors, an unreadable input file, or a header that
  is missing any of the six required columns (see below).
- Accepted records are written to `OUTPUT.jsonl`, one JSON object per line,
  **in input order**. Nothing is required on stdout. You may log a summary to
  stderr; it is not checked.

## Input

UTF-8 CSV with standard double-quote quoting (fields may contain commas,
newlines are not used inside fields, `""` escapes a quote inside a quoted
field — i.e. what Python's `csv` module reads and writes by default).

### Header

The first row is the header. Match header cells **after trimming whitespace
and lowercasing**. The six required column names are `id`, `name`, `email`,
`amount`, `date`, `tags`, **in any order**. Additionally:

- **Unknown columns are ignored.** The header may contain any number of other
  names; their cells are never read.
- **Duplicate known columns:** if a required name appears more than once, the
  **last** occurrence (rightmost) is the one that counts; earlier ones are
  ignored.
- **Missing required column:** if any of the six names is absent, the file is
  malformed — exit non-zero, write nothing.

Columns are mapped by header name, never by position.

## Per-row rules (apply in this order)

1. **Blank rows:** if every mapped field in the row is empty or
   whitespace-only, skip the row silently (neither accepted nor rejected).
2. **Trim:** strip leading/trailing whitespace from every field.
3. **id:** after trimming it must match `^[A-Za-z0-9_-]{1,32}$` (letters,
   digits, underscore, hyphen; 1–32 chars), else **reject** the row. Output
   as-is (case preserved).
4. **name:** any string (may be empty). Collapse every internal run of
   whitespace to a single space (e.g. `"Lee,\t  Ann"` → `"Lee, Ann"`). Output
   the collapsed string.
5. **email:** if empty → output JSON `null`. Otherwise:
   (a) lowercase the whole string;
   (b) it must contain **exactly one** `@` with at least one character on
       each side, else **reject**;
   (c) **plus-addressing:** in the local part (before `@`), if there is a
       `+`, delete from the first `+` through the end of the local part
       (`ann+news@ex.com` → `ann@ex.com`). The remaining local part must be
       non-empty, else **reject**;
   (d) the domain (after `@`) must contain at least one `.`, and splitting
       the domain on `.` must yield **no empty labels** (so `a@b`, `a@b.`,
       `a@.b`, `a@b..c` are all rejected).
   Output the rebuilt `local@domain`.
6. **amount:** parse with exactly this algorithm —
   (a) **parentheses negative:** if the field starts with `(` **and** ends
       with `)`, strip both and mark the value negative (accounting style);
   (b) if it now starts with `-`: if already marked negative → **reject**
       (e.g. `(-12)`); otherwise mark negative and drop the `-`;
   (c) if it now starts with `$`, drop it;
   (d) **comma groups:** if the remainder contains any `,`, the integer part
       (the substring before the first `.`, or the whole remainder if there
       is no `.`) must match `^[0-9]{1,3}(,[0-9]{3})+$` and no `,` may appear
       after the first `.`. So `1,234.50` is well-formed but `1,2,3.45`,
       `12,34`, `1234,567` and `1.2,3` are **rejected**. Then remove the
       commas;
   (e) what remains must match `^[0-9]+(\.[0-9]{1,2})?$`, else **reject**;
   (f) the numeric value, with the sign applied, must satisfy
       `|value| <= 1000000`, else **reject** (sanity cap).
   Output the signed value as a JSON number.
   Examples: `"$1,234.50"` → `1234.5`, `"(7.25)"` → `-7.25`, `"($1,000)"` →
   `-1000`, `"-$12"` → `-12`;
   `"(-5)"`, `"12.345"`, `"1,23.4"`, `"$"`, `"1.2.3"`, `"2,000,000"`, `""` →
   reject.
7. **date:** accept exactly three formats — `YYYY-MM-DD`, `MM/DD/YYYY`, or
   `DD.MM.YYYY` (all fully zero-padded, 4-digit year). It must be a real
   calendar date. Normalize to `YYYY-MM-DD`. The normalized date must fall
   inside the window **1990-01-01 .. 2035-12-31 inclusive**. Anything else
   (other separators, non-padded, impossible dates like `2025-02-30`, dates
   outside the window) → **reject** the row.
8. **tags:** split the field on **both** `;` and `|` (each is a separator),
   trim each piece, drop empty pieces, **lowercase** each piece. A piece
   that does not match `^[a-z0-9_]+$` after lowercasing is **dropped**
   (it does not reject the row). Deduplicate, sort ascending (byte order).
   If more than **10 distinct** valid tags remain, **reject** the row.
   Output the JSON array (`[]` when nothing survives).

Rejected rows are dropped silently. A rejected row never appears in the
output; rejection of one row does not affect any other row.

## Output record shape

Exactly the keys `id`, `name`, `email`, `amount`, `date`, `tags` (JSON key
order within the object does not matter). See the schema in the repo README.

## Performance budget

A 120,000-row CSV must normalize in under **45 seconds** (the grader
enforces this on a generated input). Straightforward single-pass code passes
easily; per-row work that re-scans the whole file does not.

## Starter test

```
python3 normalizer/tests/test_normalize.py
```
