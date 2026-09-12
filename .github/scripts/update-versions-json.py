"""Update gh-pages versions.json, which version-selector.js reads to populate the picker.

Usage: update-versions-json.py <version>   (run from the gh-pages checkout)

`latest` only (re)writes the "latest (main)" entry; a release tag also adds its own entry, becomes
`latest_release`, and prunes all but the newest MAX_VERSIONS releases from the file and from disk.
"""

import json
import os
import re
import shutil
import sys

MAX_VERSIONS = 5
VERSIONS_FILE = "versions.json"

version = sys.argv[1]

if os.path.exists(VERSIONS_FILE):
    with open(VERSIONS_FILE) as f:
        data = json.load(f)
else:
    data = {"latest_release": None, "versions": []}

# Rewrite the "latest (main)" entry rather than only inserting it when absent: the deployed
# versions.json predates the repo rename and still points at /time-series-store/latest/.
data["versions"] = [v for v in data["versions"] if v["version"] != "latest"]
data["versions"].insert(
    0, {"version": "latest", "label": "latest (main)", "path": "/infrastore/latest/"}
)

if version != "latest":
    # Replace any existing entry for this version (re-running a release should be idempotent).
    data["versions"] = [v for v in data["versions"] if v["version"] != version]
    data["versions"].insert(1, {"version": version, "label": version, "path": f"/infrastore/{version}/"})

    def semver_key(v):
        if v["version"] == "latest":
            return (999, 999, 999)
        # major.minor.patch only; pre-release suffixes are ignored here
        m = re.match(r"v?(\d+)\.(\d+)\.(\d+)", v["version"])
        return (int(m.group(1)), int(m.group(2)), int(m.group(3))) if m else (0, 0, 0)

    data["versions"].sort(key=semver_key, reverse=True)
    data["latest_release"] = version

    releases = [v for v in data["versions"] if v["version"] != "latest"]
    for p in releases[MAX_VERSIONS:]:
        data["versions"].remove(p)
        if os.path.isdir(p["version"]):
            shutil.rmtree(p["version"])
            print(f"Pruned old version: {p['version']}")

with open(VERSIONS_FILE, "w") as f:
    json.dump(data, f, indent=2)
    f.write("\n")

print(f"Updated versions.json: {json.dumps(data, indent=2)}")
