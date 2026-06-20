# rbx-merge

Prototype semantic diff and three-way merge tooling for Roblox `.rbxl`, `.rbxlx`, `.rbxm`, and `.rbxmx` files.

This repository contains two crates:

- `rbx_merge`: VCS-neutral backend library that decodes Roblox files, produces deterministic semantic text, and performs conservative three-way merges.
- `rbx_merge_cli`: `rbx-merge` command-line adapter for Git-style workflows.

## Library Architecture

`rbx_merge` is split into focused modules:

- `format`: file-format detection and binary/XML decode/encode.
- `semantic`: the format-independent instance model and value-equality logic.
- `identity`: cross-side instance matching (which base/ours/theirs nodes are "the same").
- `merge_graph`: the three-way merge, child-order resolution, reference-target and unique-id checks, and lowering back to a `WeakDom`.
- `render`: per-value display strings and the deterministic `textconv` tree.
- `diagnostics` / `conflict`: the reported diagnostic and conflict/report types.

The primary entry point is `merge_files(base, ours, theirs, settings)`, taking a
side-specific `FileInput { bytes, path_hint, format }` per side and returning a
`MergeReport { merged: Option<Vec<u8>>, conflicts, diagnostics }`. The older
`merge(MergeInput, MergeOptions) -> MergeResult` remains as a convenience
wrapper.

Diagnostics call out concrete locations: unknown (non-reflected) properties
preserved in the output, and ambiguous identity matches that were declined to
keep merging deterministic. References whose target was deleted in the merge are
reported as `RefTarget` conflicts rather than silently dropped.

## Conflict Resolution

When the automatic merge cannot settle a conflict, the caller supplies a
`Resolutions` value telling the merge which side to take. It carries a bulk
default (`Resolutions::take(Side::Ours)`) plus optional per-conflict overrides
keyed by conflict kind, instance path, and property
(`Resolutions::none().resolve(kind, path, property, side)`). Resolution is wired
through the property value, instance identity, parent move, child order, and
delete/modify conflicts. Any frontend — a CLI flag, an edited conflict report, a
Studio plugin — just builds a `Resolutions` and hands it to `merge_files`.

The CLI exposes both a bulk choice and a per-conflict report:

```sh
# take one side for every conflict
rbx-merge merge --base %O --ours %A --theirs %B --out %A --path %P --take ours

# write an editable report, resolve each conflict, then re-run
rbx-merge merge --base b --ours o --theirs t --out m --conflicts-out conflicts.txt
#   edit `resolution = ours|theirs|base` for each block in conflicts.txt
rbx-merge merge --base b --ours o --theirs t --out m --resolutions conflicts.txt
```

The report-driven flow needs the three inputs at resolve time, so it suits a
workflow that keeps them rather than Git's merge driver (which discards the
base/theirs temporaries once the driver exits non-zero).

## Commands

```sh
rbx-merge textconv <path>
rbx-merge merge --base <base> --ours <ours> --theirs <theirs> --out <out> --path <repo-path>
rbx-merge diff <old> <new>
```

`textconv` writes deterministic semantic text to stdout. `merge` writes the merged Roblox file only when the backend reports a clean result; conflicts are printed to stderr and the command exits nonzero. `diff` currently prints the two semantic textconv outputs with file headers.

## Git Integration

`.gitattributes`:

```gitattributes
*.rbxl  diff=rbxdom merge=rbxdom
*.rbxlx diff=rbxdom merge=rbxdom
*.rbxm  diff=rbxdom merge=rbxdom
*.rbxmx diff=rbxdom merge=rbxdom
```

Git config:

```ini
[diff "rbxdom"]
    textconv = rbx-merge textconv
    cachetextconv = true

[merge "rbxdom"]
    name = Roblox semantic merge
    driver = rbx-merge merge --base %O --ours %A --theirs %B --out %A --path %P
```

## Prototype Limitations

`WeakDom` does not model every Roblox file-level metadata field, so this prototype is semantic rather than byte-perfect. XML unknown properties are decoded with `ReadUnknown` and encoded with `WriteUnknown`; binary properties are preserved when `rbx_binary` can decode them into `WeakDom`.
