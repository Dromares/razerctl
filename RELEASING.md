# Releasing razerctl

How a release goes from `main` to a published GitHub release. Everything here is a command you run
by hand, in order. Nothing publishes itself: the workflow only ever creates a **draft** release.

The release workflow is `.github/workflows/release.yml`; it is the source of truth for how the assets are built. The
"Without hosted Actions" section below repeats its steps by hand.

Assets of a release `vX.Y.Z` (names are fixed; the in-app updater depends on them):

| Asset | Used by |
|---|---|
| `razerctl-X.Y.Z-x86_64.AppImage` | AppImage users, in-app updater |
| `razerctl-X.Y.Z-x86_64.AppImage.zsync` | AppImageUpdate, Gear Lever, AppImageLauncher |
| `razerctl-X.Y.Z-x86_64-linux.tar.gz` | manual installs |
| `70-razerctl.rules` | the AppImage `sudo curl` line in the README |
| `SHA256SUMS` | everyone; the in-app updater checks downloads against it |

## 1. Bump the version and the changelog

On a branch, open a PR, get it verified and merged to `main` like any other change:

```sh
# Cargo.toml: version = "X.Y.Z"
cargo build --release                 # rewrites razerctl's own entry in Cargo.lock; commit both files
cargo build --release --locked        # must pass: the workflow builds with --locked
```

Add a `## vX.Y.Z` section at the top of `CHANGELOG.md` (above the previous release). The workflow cuts exactly that
section out as the release notes and fails if it is missing, so write it for users: what changed, what to do. Fold any
`## Unreleased` notes into it; that heading is never cut out for a release.

## 2. Tag and push

Only after the bump is on `origin/main`:

```sh
git fetch --prune origin && git switch --detach origin/main
git tag -a vX.Y.Z -m "razerctl X.Y.Z"
git push origin vX.Y.Z
```

The tag must equal `Cargo.toml`'s `version` with a leading `v`; the workflow fails otherwise. Pushing the tag starts the
workflow, which builds on Ubuntu 22.04, runs `cargo build/clippy/test --release --locked`, and creates the **draft**
release.

## 3. Check the draft

```sh
gh release view vX.Y.Z --repo Dromares/razerctl --json isDraft,assets --jq '{isDraft, assets: [.assets[] | {name, size}]}'
mkdir -p /tmp/razerctl-vX.Y.Z && gh release download vX.Y.Z --repo Dromares/razerctl --dir /tmp/razerctl-vX.Y.Z
cd /tmp/razerctl-vX.Y.Z
sha256sum -c SHA256SUMS                         # every asset: OK
test -s razerctl-X.Y.Z-x86_64.AppImage.zsync && echo zsync non-empty
chmod +x razerctl-X.Y.Z-x86_64.AppImage
./razerctl-X.Y.Z-x86_64.AppImage --appimage-updateinformation   # gh-releases-zsync|Dromares|razerctl|latest|... (no FUSE: see the README for the objcopy line)
./razerctl-X.Y.Z-x86_64.AppImage --version                      # razerctl X.Y.Z
tar xzf razerctl-X.Y.Z-x86_64-linux.tar.gz && ./razerctl-X.Y.Z-x86_64-linux/razerctl --version
```

Check that all five assets are there, `isDraft` is `true`, the notes are the changelog section, and the version printed is
`X.Y.Z`. Do not touch the real mouse for this check (`--version` and `--help` are enough).

## 4. Publish by hand

Read the draft once more, then publish it in the GitHub web UI (Releases, the draft, make sure *Set as the latest release*
is ticked, *Publish release*) or:

```sh
gh release edit vX.Y.Z --repo Dromares/razerctl --draft=false --latest
```

`--latest` is not optional. Publishing with only `--draft=false` left the previous release as Latest (DAN-1694), and the
in-app updater (`/releases/latest`) and AppImageUpdate (`gh-releases-zsync|...|latest|...`) both follow Latest. The
workflow cannot set it, because GitHub does not let a draft be Latest.

Right after publishing, check what an anonymous client sees. This must print the new tag; if it prints the old one,
run the `gh release edit ... --latest` line again:

```sh
curl -fsS https://api.github.com/repos/Dromares/razerctl/releases/latest | grep '"tag_name"'      # "tag_name": "vX.Y.Z",
```

Publishing is the point of no return: the in-app updater and `/releases/latest/download/...` links see it immediately.

## Without hosted Actions

If the release workflow does not start (hosted Actions can fail to start on an account with a payment problem), do the
same steps by hand in an Ubuntu 22.04 root filesystem, so the binaries still run on glibc 2.35, and create the draft
yourself. Run it from a clean checkout of the tag.

```sh
# one-time: an Ubuntu 22.04 root filesystem with the build dependencies (debootstrap, systemd-nspawn on the host)
sudo debootstrap jammy /var/lib/machines/razerctl-jammy http://archive.ubuntu.com/ubuntu
sudo systemd-nspawn -D /var/lib/machines/razerctl-jammy --bind=/path/to/checkout:/src
```

Inside the root filesystem (install the same packages as `.github/workflows/ci.yml`, plus `patchelf file
desktop-file-utils squashfs-tools curl`, and a Rust toolchain as pinned by `rust-toolchain.toml`):

```sh
cd /src && git describe --tags --exact-match            # must print vX.Y.Z, and Cargo.toml must say X.Y.Z
cargo build --release --locked && cargo clippy --release --locked && cargo test --release --locked
cargo install cargo-packager --locked --version 0.11.8
cargo run --release --locked --example fetch_appimage_tools -- cargo packager --release --formats appimage
```

The AppImage lands in `target/packager/`; rename it to `razerctl-X.Y.Z-x86_64.AppImage` and its `.zsync` the same way.
Build the tarball with this layout (one top-level directory), then the checksums and the notes:

```sh
D=razerctl-X.Y.Z-x86_64-linux && mkdir -p out/$D/icons out/$D/systemd
cp target/release/razerctl target/release/razerctl-gui udev/70-razerctl.rules packaging/razerctl.desktop packaging/razerctl.svg packaging/razerctl.png LICENSE README.md out/$D/
cp packaging/icons/razerctl-*symbolic.svg out/$D/icons/ && cp systemd/*.service out/$D/systemd/
tar -C out -czf out/$D.tar.gz $D
cp udev/70-razerctl.rules out/
(cd out && sha256sum razerctl-X.Y.Z-x86_64.AppImage razerctl-X.Y.Z-x86_64.AppImage.zsync $D.tar.gz 70-razerctl.rules > SHA256SUMS)
```

Back on the host, with the changelog section saved as `notes.md` (the same section `release.yml` would use):

```sh
gh release create vX.Y.Z --repo Dromares/razerctl --draft --title "razerctl X.Y.Z" --notes-file notes.md \
    out/razerctl-X.Y.Z-x86_64.AppImage out/razerctl-X.Y.Z-x86_64.AppImage.zsync out/razerctl-X.Y.Z-x86_64-linux.tar.gz \
    out/70-razerctl.rules out/SHA256SUMS
```

Then carry on from step 3. Note in the release ticket that the release was built by hand, and why.
