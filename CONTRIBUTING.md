# Contributing

## Development setup

Clone with submodules (`rbx-test-files` is required by the test suite):

```sh
git clone --recurse-submodules https://github.com/kennethloeffler/rbx-merge.git
```

If you already cloned without them:

```sh
git submodule update --init --recursive
```

Before opening a pull request, run the same checks CI runs:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo build --all-targets --locked
cargo test --all --locked
```

## Releasing

Releases are built and published by the [Release workflow](.github/workflows/release.yml), which triggers on tags matching `v*`. It creates a **draft** GitHub release and attaches prebuilt binaries for `linux-x86_64`, `windows-x86_64`, and `macos-aarch64`.

Steps to cut a release:

1. **Bump the version** in the root `Cargo.toml` (`[workspace.package] version`). Both crates inherit it.

2. **Refresh `Cargo.lock`** so it picks up the new version:

   ```sh
   cargo check
   ```

3. **Commit and merge to `master`** via a pull request, and wait for CI to pass.

4. **Tag the release commit and push the tag.** The tag must be `v` followed by the version, e.g.:

   ```sh
   git checkout master && git pull
   git tag v0.2.0
   git push origin v0.2.0
   ```

5. **Wait for the Release workflow** to finish (Actions tab). It creates a draft release named after the tag and uploads `rbx-merge-<version>-<label>.zip` for each of the three targets.

6. **Publish the release.** Open the draft on the GitHub Releases page, verify all three archives are attached, write the release notes, and publish.

If a build job fails, fix the problem, delete the draft release and the tag (`git push --delete origin v0.2.0 && git tag -d v0.2.0`), and start again from step 4.
