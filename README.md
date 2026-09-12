# rbx-merge

Semantic diff and three-way merge for Roblox `.rbxl`, `.rbxlx`, `.rbxm`, and `.rbxmx` files.

Git normally treats Roblox place and model files as opaque blobs. `git diff` shows nothing useful, and when two branches touch the same file, git can't merge them.

rbx-merge fixes both. It decodes Roblox files into the instance tree, renders a deterministic text form for diffing, and performs a conservative three-way merge at the instance and property level.

Once set up, `git diff` on a `.rbxm` shows this instead of "Binary files differ":

```
# rbx-merge — binary model (.rbxm)
DataModel

  Folder "Folder"
    Attributes
      Boolean = Bool(true)
      Color3 = Color3(0.63529414, 0.0, 1.0)
      Number = Float64(12345.0)
      Vector3 = Vector3(1.0, 2.0, 3.0)
```

and `git merge` combines non-conflicting edits to the same place file automatically.

The repository contains two crates:

- `rbx_merge`: VCS-neutral backend library that decodes Roblox files, produces deterministic semantic text, and performs conservative three-way merges.
- `rbx_merge_cli`: the `rbx-merge` command-line adapter for git-style workflows.

## Installation

Grab a prebuilt binary from the [releases page](https://github.com/kennethloeffler/rbx-merge/releases) (Linux x86_64, Windows x86_64, macOS aarch64) and put `rbx-merge` on your `PATH`. You may use Foreman, Rokit, or any method of your choice.

Or build from source with a Rust toolchain:

```sh
cargo install --git https://github.com/kennethloeffler/rbx-merge rbx_merge_cli
```

## Setting up a repository

**1. Commit safe defaults (once, by anyone):**

```sh
rbx-merge install --write-gitattributes
git add .gitattributes && git commit -m "Mark Roblox files binary"
```

This writes a `.gitattributes` that marks Roblox files as `binary`:

```gitattributes
*.rbxl  binary
*.rbxlx binary
*.rbxm  binary
*.rbxmx binary
```

This ensures that git will never use a line-based merge for `.rbxlx` and `.rbxmx` files, which can produce valid-looking but semantically wrong output.

**2. Activate the drivers (once per clone, by every collaborator):**

```sh
rbx-merge install
rbx-merge doctor    # verify the setup at any time
```

`install` writes the driver definitions to the clone's local git config and activates them via `.git/info/attributes`. `doctor` checks the whole setup and exits nonzero if a critical piece is missing. `rbx-merge uninstall` reverts `install`.

A separate installation step is necessary for each contributor because for security reasons, git does not automatically use diff/merge drivers from any committed file.

## Day-to-day use

After setup, `git diff`, `git log -p`, and `git show` render Roblox files as the deterministic semantic text shown above. `git merge`, `git rebase`, and `git cherry-pick` run the semantic merge; when both sides' edits don't conflict, the merge completes cleanly.

By extension, this should automatically work with git clients like GitHub Desktop or GitKraken. However, some clients bypass git's textconv machinery entirely and may require further configuration to display smenatic text diffs.

You can also invoke the tooling directly, outside of git:

```sh
rbx-merge diff old.rbxm new.rbxm
rbx-merge merge --base base.rbxm --ours ours.rbxm --theirs theirs.rbxm --out merged.rbxm
```

## When a merge conflicts

If both sides changed the same property, the merge stops and asks you to choose a side. Git reports the file as conflicted, and rbx-merge saves the conflict state under `.rbxmerge/<file>/`, including an editable report, `conflicts.txt`. To resolve:

```sh
# 1. Open .rbxmerge/path/to/file.rbxmx/conflicts.txt and set
#    `resolution = ours|theirs|base` for each conflict block.

# 2. Apply your choices to the working file:
rbx-merge resolve --stash-dir .rbxmerge/path/to/file.rbxmx --out path/to/file.rbxmx

# 3. Stage the result and continue the merge as usual:
git add path/to/file.rbxmx
```

When invoking `rbx-merge merge` directly, the same choices are available:

```sh
# take one side for every conflict
rbx-merge merge --base b --ours o --theirs t --out m --take ours

# write an editable report, resolve each conflict, then re-run
rbx-merge merge --base b --ours o --theirs t --out m --conflicts-out conflicts.txt
#   edit `resolution = ours|theirs|base` for each block in conflicts.txt
rbx-merge merge --base b --ours o --theirs t --out m --resolutions conflicts.txt
```

## Global install

`rbx-merge install --global` (git ≥ 2.43) covers every repo on this machine in one step: the driver definitions go to `~/.gitconfig` and the activation to git's global attributes file (`core.attributesFile`, defaulting to `~/.config/git/attributes`). Nothing repo-local is written, and `rbx-merge uninstall --global` reverts only the global install.

Keep in mind:

- It applies to every git repo on this machine, including clones of projects that never adopted rbx-merge.
- git treats the global attributes with lowest precedence: any committed `.gitattributes` rule overrides it. In particular, repos committing the recommended `binary` safe default still need a per-clone `rbx-merge install`, whose `.git/info/attributes` override outranks the committed file. `doctor` reports which case a clone is in.

## Command reference

```sh
rbx-merge textconv <path>
rbx-merge merge --base <base> --ours <ours> --theirs <theirs> --out <out> --path <repo-path>
rbx-merge resolve --stash-dir <dir> --out <path>
rbx-merge diff <old> <new>
rbx-merge install [--global] [--no-stash] [--write-gitattributes] [--driver-path <exe>]
rbx-merge doctor
rbx-merge uninstall [--global]
```

`textconv` writes deterministic semantic text to stdout. `merge` writes the merged Roblox file only when the backend reports a clean result; conflicts are printed to stderr and the command exits nonzero. `resolve` re-runs a conflicted merge from a `--stash-dir` after its `conflicts.txt` has been edited. `diff` currently prints the two semantic textconv outputs with file headers.

## Limitations

`WeakDom` does not model every Roblox file-level metadata field, so the merge is semantic rather than byte-perfect. XML unknown properties are decoded with `ReadUnknown` and encoded with `WriteUnknown`; binary properties are preserved when `rbx_binary` can decode them into `WeakDom`.
