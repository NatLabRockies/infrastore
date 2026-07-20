# Format (or check) the Julia sources in this repository.
#
# Usage, from the repository root:
#   julia --project=julia/formatter julia/formatter/format.jl          # rewrite in place
#   julia --project=julia/formatter julia/formatter/format.jl --check  # exit 1 if unformatted
using JuliaFormatter

const PACKAGE_DIR = normpath(joinpath(@__DIR__, "..", "TimeSeriesStore.jl"))
const check_only = "--check" in ARGS

formatted = format(PACKAGE_DIR; overwrite=!check_only)
if check_only && !formatted
    @error "Julia sources are not formatted. Run: julia --project=julia/formatter julia/formatter/format.jl"
    exit(1)
end
