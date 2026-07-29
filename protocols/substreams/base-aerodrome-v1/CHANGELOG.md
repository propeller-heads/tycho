# Changelog

## v0.1.2

- Add the package to the substreams workspace. It was missing from the root
  `[workspace.members]`, so cargo refused to build it ("current package
  believes it's in a workspace when it's not"). Dependencies now resolve from
  the shared workspace lockfile.

## v0.1.1

- Bump the package version for the release.

## v0.1.0

- Add the Aerodrome V1 Substreams integration for Base.
