"""Guard against drift between the runtime module and infrastore.pyi.

Every public runtime name must appear in the stub, and every public method of
every stubbed class must exist at runtime (and vice versa). Signatures are not
compared — the stub is hand-written — but name-level drift is caught here.
"""

import ast
from pathlib import Path

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
STUB_ONLY = {"Period", "RequestedType", "TimeSeriesData"}


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
