# Horizon build (x86-64 Linux, `fast` profile)

Built from `claude/busy-edison-kk682k` at 4cc50b5257 of Ismmedero9/horizon, with:
- the straight-down glide fix already on main (9a8a40b016, table trig for `updateFallFlyingMovement`),
- creative slots sent only while the creative screen is open (244a0a3),
- a test pinning the glide step to the 26.3 jar (4cc50b5).

Command: `cargo build --profile fast -p horizon-game --bin horizon-client`, then `strip`.
Dynamically linked (glibc, ALSA, udev), so it needs the same system libraries the release uses.

## Use
    xz -dk horizon-client.xz && chmod +x horizon-client
    sha256sum -c SHA256SUMS            # the decompressed binary must match
    ./horizon-client --resources <your prepared>/26.3

It contains no Mojang files; it reads your own prepared resources. Not a signed release.
