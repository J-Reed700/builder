# Releases

Pushing a `v*` tag runs the release workflow. Formatting, workspace Clippy and
tests, retrieval checks, browser recovery, terminal regressions, and the Docker
gateway round trip must pass before target builds start. The Linux, macOS, and
Windows archives and SHA-256 files are then attached to the GitHub Release.

Running the workflow manually performs the same quality gates and target builds,
and leaves the archives as workflow artifacts. It does not create a GitHub
Release because there is no release tag. The gateway image job retains its
existing behavior and publishes its metadata tags to GHCR after the quality
gates pass.
