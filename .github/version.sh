#!/bin/sh
# Prints the version used in package file names, as GITHUB_ENV lines.
# Release tags (v0.1.0) give "0.1.0"; every other build adds the commit, "0.1.0-4011f23".
set -e
cargo=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
case "$GITHUB_REF" in
  refs/tags/v*) version=$cargo ;;
  *) version="$cargo-$(echo "${GITHUB_SHA:-$(git rev-parse HEAD)}" | cut -c1-7)" ;;
esac
echo "CARGO_VERSION=$cargo"
echo "COPYM_VERSION=$version"
