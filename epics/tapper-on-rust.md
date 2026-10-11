# Epic: Tapper on Rust — limen's part

> Not a docs page. This file is a pointer plus the rules that apply in this
> repo, never a copy of the roadmap. limen and pharos are the tools the epic's
> goal 3 sharpens for the ~50 JVM (Kotlin/Java, Ratpack) work migrations;
> slauth (see `docs/guides/adoption.md`) and tapper are the real services they
> have been proven on so far.

**Why this exists, read first:**
https://claude.ai/code/artifact/684f3645-0e5d-42dd-a1f6-2fab6e094e44
(Private: it opens with the owner's claude.ai login. A 404 from anywhere else
is expected, not a broken link.)
Sections that matter here:
- §01, the four goals and their order (goal 3 is this repo's);
- §02, why the comparison needs two live servers, and "the deadline that
  remains" (any tool change that needs proof against two live servers lands
  before tapper's Python server is deleted);
- §03, the work table's repo column (an issue is filed where its merge
  happens);
- §06, the cold-start dry run, and why the first work prototype decides what
  gets promoted;
- §08, the method rules (probe before designing; build comparison data
  separately from the code under test; break every check on purpose; treat an
  empty review as a failure).

**Tracking:** https://github.com/users/charliek/projects/4, `Epic: Tapper on Rust`.
Your PR body must contain `Closes charliek/limen#<n>`. Status moves by itself
when that merges. Never edit board status by hand.

## This repo's items

The issue is authoritative; this table is a map.

```bash
gh issue list -R charliek/limen --state open --search "in:title [W"
```

| ID | issue | phase | one line |
|---|---|---|---|
| W4a | [#31](https://github.com/charliek/limen/issues/31) | TR/W4 | cold-start dry run: a fresh agent sets up a migration from the limen + pharos docs alone; stalls become docs fixes or issues |

The dry run's tool follow-ups (limen#32–#36, pharos#16–#21) are backlog, not
epic items.

**Order:**
- W4a runs before the work prototypes (week of 2026-10-12). What the first
  prototype actually needs from tapper's traffic driver is promoted afterwards
  (that is tapper's W4b, tapper#68), not predicted here.

## Rules that apply in this repo

1. **Twinned docs.** `docs/{limen_spec,pharos_spec,prfaq,runbook}.md` are
   byte-identical in limen and pharos (CI: `docs-twin`). Edit both or neither:
   change the upstream of record (limen for `limen_spec.md`, `runbook.md`,
   `prfaq.md`; pharos for `pharos_spec.md`), byte-copy it, and land the copy
   in the same session. `docs-twin` on a PR compares against the sibling's
   `main`, so it is expected red until the sibling PR merges; after both
   merge, `workflow_dispatch` docs-twin on both mains and confirm green.
2. **A skip counts against the gate** (limen#27, the B1 rule). Never silently
   narrow a gate or a floor to make a run green; an exemption is explicit
   (`min_comparisons: 0`) and visible.
3. **Docs written for newcomers teach a general method**, never one test
   service's specifics, and every claim is checked against the code
   (file:line) before it lands. (This is how W4a's fixes were gated.)
4. **Prove a tool change against two live servers before tapper's Python
   server is deleted** (artifact §02; tapper's W5a deletes it).
5. **Ports on the owner's machine:** tapper's local backends own
   8100/8101/8200/8300 until W5a; bind limen experiments elsewhere (W4a used
   9100–9199).

## Cross-repo edges

- **pharos** — the twinned docs above, and the lockstep fixture:
  `tests/lockstep/` here is a byte-identical twin of pharos's
  `tests/fixtures/lockstep/`, checked by pharos CI's `lockstep-twin` job. A
  change to merge or comparison semantics lands in both repos together.
- **tapper** — W0 (tapper#70) pins limen/pharos SHAs, so a limen change
  reaches tapper only through a new pin. W4b (tapper#68) promotes the generic
  core of tapper's traffic driver, driven by the first work prototype. If
  tapper's W2a (tapper#76) finds pharos cannot express a Rust-only
  expectation, that becomes a pharos PR, pinned in tapper afterwards.
