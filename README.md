# rbx-merge

Prototype semantic diff and three-way merge tooling for Roblox `.rbxl`, `.rbxlx`, `.rbxm`, and `.rbxmx` files.

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

## Prototype Limitations

`WeakDom` does not model every Roblox file-level metadata field, so this prototype is semantic rather than byte-perfect. XML unknown properties are decoded with `ReadUnknown` and encoded with `WriteUnknown`; binary properties are preserved when `rbx_binary` can decode them into `WeakDom`.
