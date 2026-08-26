"""Guard against drift between the runtime module and infrastore.pyi.

Every public runtime name must appear in the stub, and every public method of
every stubbed class must exist at runtime (and vice versa). Signatures are not
compared — the stub is hand-written — but name-level drift is caught here.
"""

import ast
from pathlib import Path

import pytest

import infrastore

STUB_PATH = (
    Path(__file__).resolve().parents[2]
    / "crates"
    / "infrastore-py"
    / "infrastore.pyi"
)


def load_stub():
    tree = ast.parse(STUB_PATH.read_text())
    classes: dict[str, set[str]] = {}
    functions: set[str] = set()
    assigns: set[str] = set()
    for node in tree.body:
        if isinstance(node, ast.ClassDef):
            members = set()
            for item in node.body:
                if isinstance(item, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    members.add(item.name)
                elif isinstance(item, ast.AnnAssign) and isinstance(item.target, ast.Name):
                    members.add(item.target.id)
            classes[node.name] = members
        elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            functions.add(node.name)
        elif isinstance(node, ast.AnnAssign) and isinstance(node.target, ast.Name):
            assigns.add(node.target.id)
        elif isinstance(node, ast.Assign):
            for target in node.targets:
                if isinstance(target, ast.Name):
                    assigns.add(target.id)
    return classes, functions, assigns


STUB_CLASSES, STUB_FUNCTIONS, STUB_ASSIGNS = load_stub()
# Type aliases in the stub with no runtime counterpart.
STUB_ONLY = {"Period", "TimeSeriesData"}


def public_runtime_names():
    # `infrastore` is the native extension's self-reference inside the
    # maturin-generated package, not part of the API surface.
    return {n for n in dir(infrastore) if not n.startswith("_") and n != "infrastore"}


def test_every_runtime_name_is_stubbed():
    stubbed = set(STUB_CLASSES) | STUB_FUNCTIONS | STUB_ASSIGNS | {"__version__"}
    missing = public_runtime_names() - stubbed
    assert not missing, f"public runtime names missing from the stub: {sorted(missing)}"


def test_every_stub_name_exists_at_runtime():
    stubbed = (set(STUB_CLASSES) | STUB_FUNCTIONS | STUB_ASSIGNS) - STUB_ONLY
    ghosts = {n for n in stubbed if not hasattr(infrastore, n)}
    assert not ghosts, f"stubbed names that do not exist at runtime: {sorted(ghosts)}"


def test_class_members_match():
    problems = []
    for cls_name, stub_members in STUB_CLASSES.items():
        runtime_cls = getattr(infrastore, cls_name, None)
        if runtime_cls is None or not isinstance(runtime_cls, type):
            continue
        if issubclass(runtime_cls, BaseException):
            continue
        runtime_members = {m for m in dir(runtime_cls) if not m.startswith("_")}
        stub_public = {m for m in stub_members if not m.startswith("_")}
        missing_in_stub = runtime_members - stub_public
        ghost_in_stub = stub_public - runtime_members
        if missing_in_stub:
            problems.append(f"{cls_name}: runtime members missing from stub: {sorted(missing_in_stub)}")
        if ghost_in_stub:
            problems.append(f"{cls_name}: stub members not present at runtime: {sorted(ghost_in_stub)}")
    assert not problems, "\n".join(problems)


# ---- signature drift -------------------------------------------------------
#
# Name-level checks above catch a member that appears or vanishes. They cannot
# see a stub whose *arguments* disagree with the runtime's -- a promised keyword
# that raises TypeError, an argument the stub renamed, one that moved from
# positional to keyword-only. Both kinds have happened here: the stub advertised
# `build_static_reader(time_series_type=...)` accepting a str while the runtime
# refused one, and `__exit__`'s arguments were `_exc_type` / `_exc_value` /
# `_traceback` at runtime against the stub's unprefixed names.
#
# Types and defaults' *values* are still not compared -- the stub is hand-written
# and deliberately more precise than anything the runtime exposes. Argument
# names, their order, their kind, and whether they have a default are compared,
# because those are what a caller writes.

import inspect


def stub_signatures():
    """{(class, method): [(name, kind, has_default)]} for every stubbed method."""
    tree = ast.parse(STUB_PATH.read_text())
    out: dict[tuple[str, str], list[tuple[str, str, bool]]] = {}
    for node in tree.body:
        if not isinstance(node, ast.ClassDef):
            continue
        for item in node.body:
            if not isinstance(item, ast.FunctionDef):
                continue
            # A property is an attribute to the caller, not a call.
            if any(
                isinstance(d, ast.Name) and d.id == "property" for d in item.decorator_list
            ):
                continue
            a = item.args
            params: list[tuple[str, str, bool]] = []
            pos = a.posonlyargs + a.args
            pad = len(pos) - len(a.defaults)
            for i, arg in enumerate(pos):
                # `inspect.signature` of a bound method or a classmethod drops
                # the receiver; the stub spells it out.
                if arg.arg in ("self", "cls"):
                    continue
                params.append((arg.arg, "positional", i >= pad))
            for arg, default in zip(a.kwonlyargs, a.kw_defaults):
                params.append((arg.arg, "keyword", default is not None))
            out[(node.name, item.name)] = params
    return out


def runtime_signature(cls, method):
    """The same shape from the runtime, or None when it exposes no signature."""
    target = cls if method == "__init__" else getattr(cls, method, None)
    if target is None:
        return None
    try:
        sig = inspect.signature(target)
    except (ValueError, TypeError):
        return None
    kinds = {
        inspect.Parameter.POSITIONAL_ONLY: "positional",
        inspect.Parameter.POSITIONAL_OR_KEYWORD: "positional",
        inspect.Parameter.KEYWORD_ONLY: "keyword",
    }
    params = []
    for name, p in sig.parameters.items():
        if name == "self":
            continue
        if p.kind not in kinds:  # *args / **kwargs carry no names to compare
            return None
        params.append((name, kinds[p.kind], p.default is not inspect.Parameter.empty))
    return params


def test_method_signatures_match():
    problems = []
    for (cls_name, method), stub_params in stub_signatures().items():
        runtime_cls = getattr(infrastore, cls_name, None)
        if runtime_cls is None or not isinstance(runtime_cls, type):
            continue
        if issubclass(runtime_cls, BaseException):
            continue
        runtime_params = runtime_signature(runtime_cls, method)
        if runtime_params is None:
            continue
        if runtime_params != stub_params:
            problems.append(
                f"{cls_name}.{method}:\n"
                f"    stub:    {stub_params}\n"
                f"    runtime: {runtime_params}"
            )
    assert not problems, "signature drift between the stub and the runtime:\n" + "\n".join(
        problems
    )


# ---- metadata-dict drift ---------------------------------------------------
#
# `get_metadata` returns a plain dict, so the stub says `dict[str, Any]` and the
# name-level checks above cannot see a field that never made it in. That is not
# hypothetical: `association_id` reached the OpenAPI export row, the C ABI, and
# Julia while Python's metadata dict silently omitted it.
#
# The catch is that the export row and the metadata dict are *independent*
# renderings of the same core `TimeSeriesMetadata`, one in `openapi.rs` and one
# in `metadata_to_dict`. Every column the row carries must therefore also be
# reachable from the dict, so a field threaded into one renderer and not the
# other shows up here. `uri` is the row's own addition — it names the store, not
# a catalog column — and is the single documented exception.

import json
from datetime import datetime, timedelta, timezone

import numpy as np

import infrastore as _is

_T0 = datetime(2030, 1, 1, tzinfo=timezone.utc)
_HOUR = timedelta(hours=1)
ROW_ONLY_KEYS = {"uri"}


def _store_with_every_shape():
    store = _is.Store.create(in_memory=True)
    common = {
        "owner_type": "Generator",
        "owner_category": _is.OwnerCategory.Component,
    }
    store.add_time_series(
        owner_id=1,
        time_series=_is.SingleTimeSeries(
            initial_timestamp=_T0, resolution=_HOUR, data=np.arange(3.0), name="sts"
        ),
        **common,
    )
    store.add_time_series(
        owner_id=2,
        time_series=_is.Deterministic(
            initial_timestamp=_T0,
            resolution=_HOUR,
            horizon=3 * _HOUR,
            interval=_HOUR,
            count=3,
            data=np.arange(9.0).reshape(3, 3),
            name="fc",
        ),
        **common,
    )
    store.add_time_series(
        owner_id=3,
        time_series=_is.NonSequentialTimeSeries(
            timestamps=[_T0, _T0 + _HOUR, _T0 + 3 * _HOUR],
            data=np.arange(3.0),
            name="nsts",
        ),
        **common,
    )
    return store


def test_metadata_dict_covers_every_exported_row_field():
    store = _store_with_every_shape()
    rows = {r["name"]: r for r in json.loads(store.export_time_series_associations_openapi())}
    problems = []
    for key in store.list_keys():
        meta = store.get_metadata(key)
        row = rows[meta["name"]]
        missing = set(row) - ROW_ONLY_KEYS - set(meta)
        if missing:
            problems.append(
                f"{meta['name']}: exported row fields absent from get_metadata: {sorted(missing)}"
            )
    assert not problems, "\n".join(problems)


def test_metadata_dict_carries_the_association_id():
    """The specific field this guard was added for — and the by-id getter that
    makes it useful, which core, the C ABI, and Julia had before Python did."""
    store = _store_with_every_shape()
    for key in store.list_keys():
        meta = store.get_metadata(key)
        assoc_id = meta["association_id"]
        assert isinstance(assoc_id, int) and assoc_id > 0
        assert store.get_metadata_by_association_id(assoc_id) == meta


def test_get_metadata_by_association_id_rejects_an_unknown_id():
    store = _store_with_every_shape()
    known = {store.get_metadata(k)["association_id"] for k in store.list_keys()}
    with pytest.raises(_is.NotFoundError):
        store.get_metadata_by_association_id(max(known) + 1000)
