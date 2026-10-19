# Changelog

## v0.1.3

- Bump the Rust toolchain pin from 1.75 to 1.96.0. Cargo 1.75 cannot parse the
  workspace's version-4 `Cargo.lock`, so release builds from the package
  directory failed. The 1.75 pin was never honored in CI anyway — releases
  built from the workspace root with current stable.
