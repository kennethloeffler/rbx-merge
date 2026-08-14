# rbx-merge

Semantic diff and three-way merge tooling for Roblox `.rbxl`, `.rbxlx`, `.rbxm`, and `.rbxmx` files.

This repository contains two crates:

- `rbx_merge`: VCS-neutral backend library that decodes Roblox files, produces deterministic semantic text, and performs conservative three-way merges.
- `rbx_merge_cli`: `rbx-merge` command-line adapter for Git-style workflows.

## Commands

```sh
rbx-merge textconv <path>
rbx-merge merge --base <base> --ours <ours> --theirs <theirs> --out <out> --path <repo-path>
rbx-merge resolve --stash-dir <dir> --out <path>
rbx-merge diff <old> <new>
rbx-merge install [--global] [--stash] [--write-gitattributes] [--driver-path <exe>]
rbx-merge doctor
rbx-merge uninstall [--global]
```

`textconv` writes deterministic semantic text to stdout. `merge` writes the merged Roblox file only when the backend reports a clean result; conflicts are printed to stderr and the command exits nonzero. `resolve` re-runs a conflicted merge from a `--stash-dir` after its `conflicts.txt` has been edited; see [Conflict Resolution](#conflict-resolution). `diff` currently prints the two semantic textconv outputs with file headers.

`install` configures the current clone to use the diff and merge drivers, `doctor` checks that setup and exits nonzero when a critical piece is missing, and `uninstall` reverts `install`. See [Git Integration](#git-integration).

## Git Integration

Run once per clone:

```sh
rbx-merge install          # this clone; --global keeps the driver definitions
                           # in ~/.gitconfig instead of this repo's config
rbx-merge doctor           # verify the current clone at any time
rbx-merge uninstall        # revert install for this clone
```

### Why a committed `.gitattributes` isn't enough

Git lets a repository *name* a merge/diff driver in a committed `.gitattributes`, but for security it refuses to let the repository *define* the command that driver runs, that must live in Git config or `.git/info/attributes`, neither of which `git clone` copies. So every collaborator needs the one-time `install`.

Until they run it, you do not want Git falling back to a line-based merge of these structured files: for `.rbxlx`/`.rbxmx` a "clean" line merge can produce a valid-looking but semantically wrong file. Commit them as `binary` instead, so an uninstalled clone gets a loud conflict that keeps its own side rather than a silent corruption:

```gitattributes
# .gitattributes — committed; the safe default before anyone runs install
*.rbxl  binary
*.rbxlx binary
*.rbxm  binary
*.rbxmx binary
```

`rbx-merge install --write-gitattributes` writes exactly this block. `install` then layers the active drivers on top via the higher-precedence `.git/info/attributes` (written per clone, never committed):

```gitattributes
# .git/info/attributes — written by install
*.rbxl  merge=rbxdom diff=rbxdom
*.rbxlx merge=rbxdom diff=rbxdom
*.rbxm  merge=rbxdom diff=rbxdom
*.rbxmx merge=rbxdom diff=rbxdom
```

### Under the hood

`install` writes the following Git config (`--local` by default, `--global` to define the drivers once per machine. The `.git/info/attributes` override is always per clone, so `install` must still be run in each clone):

```ini
[diff "rbxdom"]
    textconv = rbx-merge textconv
    cachetextconv = true

[merge "rbxdom"]
    name = Roblox semantic merge (rbx-merge)
    driver = rbx-merge merge --base %O --ours %A --theirs %B --out %A --path %P
```

Pass `install --stash` to use the stash-based driver (`--stash-dir .rbxmerge/%P`, and gitignore `.rbxmerge/`) so conflicts survive Git discarding its temporaries. See [Conflict Resolution](#conflict-resolution).

For worktrees: `install` writes the override to the repository's shared `.git/info/attributes`, so it applies to every worktree. `rbx-merge uninstall` reverts the per-clone pieces (config entries, attributes override, and `.rbxmerge/` ignore) but leaves any committed `.gitattributes` in place.

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
