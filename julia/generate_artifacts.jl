# Regenerates julia/InfraStore.jl/Artifacts.toml from a published GitHub
# Release. Run from the repository root after publishing the release:
#
#   julia julia/generate_artifacts.jl v0.6.0
#
# Downloads each platform tarball, computes its archive hash (sha256) and the
# tree hash of the unpacked directory (git-tree-sha1 -- the one Pkg keys on;
# they are different hashes), and rewrites Artifacts.toml. A draft release
# will not work: its asset URLs are not publicly downloadable, so publish
# first.

using Pkg
Pkg.activate(; temp=true)
Pkg.add(; name="ArtifactUtils", version="0.2")

using ArtifactUtils
using Base.BinaryPlatforms: Platform

isempty(ARGS) && error("usage: julia julia/generate_artifacts.jl vX.Y.Z")
version = ARGS[1]
startswith(version, "v") || error("expected a tag like v0.6.0, got $(version)")

const BASE = "https://github.com/NatLabRockies/infrastore/releases/download"
out = joinpath(@__DIR__, "InfraStore.jl", "Artifacts.toml")

# Triplet -> Pkg platform keys. Must stay in sync with release.yml's
# `artifact_triplet` matrix values.
platforms = [
    ("x86_64-linux-gnu", Platform("x86_64", "linux"; libc="glibc")),
    ("aarch64-linux-gnu", Platform("aarch64", "linux"; libc="glibc")),
    ("x86_64-apple-darwin", Platform("x86_64", "macos")),
    ("aarch64-apple-darwin", Platform("aarch64", "macos")),
    ("x86_64-w64-mingw32", Platform("x86_64", "windows")),
]

isfile(out) && rm(out)
for (triplet, platform) in platforms
    url = "$(BASE)/$(version)/libinfrastore_ffi.$(triplet).tar.gz"
    @info "adding" triplet url
    add_artifact!(out, "libinfrastore_ffi", url; platform=platform, force=true, lazy=false)
end
@info "wrote" out
