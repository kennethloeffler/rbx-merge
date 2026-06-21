# rbx-merge

Semantic diff and three-way merge tooling for Roblox `.rbxl`, `.rbxlx`, `.rbxm`, and `.rbxmx` files.

This repository contains two crates:

- `rbx_merge`: VCS-neutral backend library that decodes Roblox files, produces deterministic semantic text, and performs conservative three-way merges.
- `rbx_merge_cli`: `rbx-merge` command-line adapter for Git-style workflows.

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

## Conflict Resolution

When the automatic merge cannot settle a conflict, the caller chooses which side
to take. The CLI exposes both a bulk choice and a per-conflict report:

```sh
# take one side for every conflict
rbx-merge merge --base %O --ours %A --theirs %B --out %A --path %P --take ours

# write an editable report, resolve each conflict, then re-run
rbx-merge merge --base b --ours o --theirs t --out m --conflicts-out conflicts.txt
#   edit `resolution = ours|theirs|base` for each block in conflicts.txt
rbx-merge merge --base b --ours o --theirs t --out m --resolutions conflicts.txt
```

Under Git, the base/theirs temporaries are discarded once the driver exits
non-zero, so the driver can stash everything it needs to resolve later:

```ini
[merge "rbxdom"]
    driver = rbx-merge merge --base %O --ours %A --theirs %B --out %A --path %P --stash-dir .rbxmerge/%P
```

On conflict this writes `.rbxmerge/<file>/{base,ours,theirs,path,conflicts.txt}`.
Edit `conflicts.txt`, then re-merge from the stash into the working file:

```sh
rbx-merge resolve --stash-dir .rbxmerge/path/to/file.rbxmx --out path/to/file.rbxmx
git add path/to/file.rbxmx
```

## Library

`rbx_merge` is the VCS-neutral backend the CLI is built on. Its primary entry
point is `merge_files(base, ours, theirs, settings)`, which takes a `FileInput`
per side and returns a `MergeReport { merged, conflicts, diagnostics }`.
Conflict resolution is data-driven: a frontend builds a `Resolutions` value
describing which side to take and hands it to the merge.

See the [crate documentation](https://docs.rs/rbx_merge) for the module
architecture, diagnostics, and the resolution model.

## Limitations

`WeakDom` does not model every Roblox file-level metadata field, so the merge is semantic rather than byte-perfect. XML unknown properties are decoded with `ReadUnknown` and encoded with `WriteUnknown`; binary properties are preserved when `rbx_binary` can decode them into `WeakDom`.
