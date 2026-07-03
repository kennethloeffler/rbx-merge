# rbx_merge Identity Matching and Merge Architecture

## Why a semantic merge

Roblox files (`.rbxl`, `.rbxlx`, `.rbxm`, `.rbxmx`) are not line-oriented, so a
textual three-way merge is meaningless on them. They encode a *tree of
instances*, each with a class, a name, an ordered list of children, and a bag of
typed properties. Some properties are cross-references that point at other
instances in the same file by an opaque, file-local referent.

A correct merge therefore operates on the tree. The core steps are:

1. **Decode** each side's bytes into an in-memory DOM.
2. **Normalize** each DOM into a format-independent semantic model.
3. **Match identities** across the three sides, deciding which instances are "the
   same" instance.
4. **Merge** the matched instances field by field, then run whole-graph validity
   checks.
5. **Lower** the merged result back into a DOM and **encode** it.

Conflicts and diagnostics are returned to the caller rather than written inline;
merged bytes are produced only when the merge is completely clean.

## The semantic model

Normalization assigns every instance a dense, deterministic id in tree order and
records its class, name, parent, children, and typed properties, along with
enough information to rewrite cross-references at the end of the merge.

Two ideas in this layer matter downstream:

- **Order-insensitive, stable value equality.** Values are compared by a key
  that is stable across files: floats by their exact bits, large binary blobs by
  hash, attribute bags key by key.
- **Reference-aware equality.** A cross-reference is compared not by its raw
  (meaningless across files) referent but by the *merge identity* it resolves
  to.  Two sides that both point "at the same instance" compare equal even
  though their raw referents may differ. This is why identity matching must run
  before any property comparison.

## Identity matching

Matching produces a set of rows, each row one logical instance holding up to one
per-side id (base, ours, theirs). It is **base-anchored** and layers signals from
strongest to weakest, with "first writer wins": once an instance is claimed, a
weaker heuristic cannot steal it.

- **Roots** match unconditionally.
- **Stable unique ids.** A unique identity present on exactly one instance per
  side is an exact match regardless of name, class, parent, or position.
  Ambiguous groups (several instances sharing a regenerated id) are deliberately
  left to weaker passes.
- **Singletons with stable identity but no unique id** (e.g. services) are
  matched by class.
- **Structural fixpoint.** Starting from matched parents, children are matched by
  position within `(class, name)` groups - recovering identity that a naive
  delete-plus-add would lose - and a one-to-one, sufficiently-similar pair under
  a class may be recovered as a rename. Renames and multi-member positional
  pairings are recorded as diagnostics.

Additions are reconciled too: an addition made independently on both sides is
unified into a single row when it can be matched unambiguously, and reported
rather than guessed when it cannot.

The output lets every later stage ask only "what changed?", never "the same?".

## The three-way merge

Each row is decided independently and the graph is reconciled into a valid tree
afterward. The unit of merge is a single field, governed by the classic
three-way rule:

> If both sides equal base, keep base. If only one side changed, take it. If both
> changed alike, take that. If both changed differently, it is a conflict.

Per row, in order: decide deletion (a delete vs. a delete/modify conflict),
merge class and name, merge parent, merge properties, and assign a stable output
referent. Each merged value remembers which side it came from so its references
can later be rewritten through that side's mapping.

Two property cases are special: attribute bags are merged recursively, key by
key, so independent attribute edits compose; and regenerated identity metadata
that diverges three ways is resolved deterministically rather than raised as a
conflict the user cannot act on.

### Whole-graph validity

Individually-clean field merges can still combine into an invalid tree, so a
suite of whole-graph checks runs afterward. Each can both *resolve* (given a
caller resolution) and *report*:

- **Parent cycles**: independent reparents that form a loop no tree can satisfy.
- **Unique-id collisions**: two surviving instances sharing one id.
- **Dangling references**: a surviving reference whose target did not survive.

If any conflict remains, the merge returns early with no merged bytes. A partial
file is never written.

### Finishing a clean merge

Only a conflict-free graph proceeds: child order is resolved (cleanly merging
independent insertions where both sides preserve base's relative order),
remaining diagnostics are emitted (e.g. references silently nilled by a
deletion, properties unknown to the reflection database), and the graph is
lowered back to a DOM with every reference rewritten to its final referent. The
encoded bytes are returned alongside the accumulated diagnostics.

## Conflict resolution model

Resolution is **data-driven and frontend-agnostic**. A caller builds a
resolutions value (an optional bulk default plus per-conflict overrides) and
hands it to the merge, which consults it at every decision point that can
conflict. The same value drives a CLI flag, an edited conflict report, or a
Studio plugin without any of them knowing about the others. For structural
conflicts where a side choice is not meaningful, resolving simply drops the
offending id or reference.

## Invariants the algorithms maintain

- **Identity is decided once, up front.** No later stage re-asks whether two
  instances are the same; it only consults the matched rows.
- **Determinism.** The output is a pure function of the three inputs:
  deterministic ids, insertion-ordered structures, sorted diagnostics, and a
  refusal to guess on ambiguous matches.
- **No silent corruption.** Every way the merge could drop or dangle an instance
  or reference is either reported as a conflict (blocking output) or surfaced as
  a diagnostic. A file is written only when the entire graph is clean.
- **Conservatism.** When the merge cannot decide, it declines and reports rather
  than guessing, leaving the choice to the caller.
