#!/usr/bin/env python3
"""Network-free workspace tree and transfer assertions for visual E2Es."""

# @lat: [[test#Test Harness#Visual E2E Tests#Workspace IPC on the wire]]
# @lat: [[test#GPUI Workspace Drag]]
import json
import sys
import time


def rows(path):
    try:
        with open(path) as file:
            for line in file:
                try:
                    yield json.loads(line)
                except ValueError:
                    pass
    except OSError:
        return


def message(row):
    return row.get("message", {})


def trees(path):
    for row in rows(path):
        current = message(row)
        if row.get("dir") == "client" and current.get("type") == "ReportWorkspaceTree":
            yield current.get("tree")
        if row.get("dir") == "server" and current.get("type") == "SessionList" and current.get("workspace_tree"):
            yield current.get("workspace_tree")


def latest_tree(path):
    found = list(trees(path))
    return found[-1] if found else None


def leaves(node):
    if not isinstance(node, dict):
        return []
    if "Leaf" in node:
        return [node["Leaf"]]
    split = node.get("Split", node)
    return leaves(split.get("first")) + leaves(split.get("second"))


def direction(node):
    return node.get("Split", node).get("direction") if isinstance(node, dict) else None


def count(path, name):
    return sum(message(row).get("type") == name for row in rows(path))


def counts(path):
    found = {}
    for row in rows(path):
        name = message(row).get("type")
        found[name] = found.get(name, 0) + 1
    return found


def wait_leaves(path, wanted, require_sessions):
    deadline = time.time() + 30
    while time.time() < deadline:
        found = leaves(latest_tree(path))
        if len(found) == wanted and (not require_sessions or all(leaf.get("session_ids") for leaf in found)):
            print(" ".join(str(leaf["workspace_id"]) for leaf in found))
            return 0
        time.sleep(0.2)
    return 1


def wait_root(path, wanted_direction, first, second):
    deadline = time.time() + 30
    while time.time() < deadline:
        tree = latest_tree(path)
        found = leaves(tree)
        if direction(tree) == wanted_direction and [str(leaf["workspace_id"]) for leaf in found] == [first, second]:
            print(json.dumps(tree, separators=(",", ":")))
            return 0
        time.sleep(0.2)
    print(json.dumps(latest_tree(path), separators=(",", ":")), file=sys.stderr)
    return 1


def leaf(path, wanted):
    for item in leaves(latest_tree(path)):
        if str(item.get("workspace_id")) == wanted:
            print(json.dumps(item, sort_keys=True, separators=(",", ":")))
            return 0
    return 1


def leaf_session_count(path, wanted):
    for item in leaves(latest_tree(path)):
        if str(item.get("workspace_id")) == wanted:
            print(len(item.get("session_ids", [])))
            return 0
    print(0)
    return 0


def wait_report(path):
    deadline = time.time() + 25
    while time.time() < deadline:
        if count(path, "ReportWorkspaceTree"):
            print(json.dumps(latest_tree(path), sort_keys=True, separators=(",", ":")))
            return 0
        time.sleep(0.2)
    return 1


def assert_transfer(path, wanted, leaf_path, source_workspace=None):
    with open(leaf_path) as file:
        source_leaf = json.load(file)
    source_sessions = set(source_leaf.get("session_ids", []))
    result = source = target = False
    creates = 0
    created = set()
    known = set()
    transfer_target = None
    expected_leaf = json.dumps(source_leaf, sort_keys=True, separators=(",", ":"))

    for row in rows(path):
        current = message(row)
        if row.get("dir") == "client" and current.get("type") == "TransferWorkspace":
            transfer_target = current.get("target_window_id")
        if row.get("dir") == "client" and current.get("type") == "CreateSession":
            creates += 1
        if row.get("dir") == "server" and current.get("type") == "SessionCreated":
            created.add(current.get("session_id"))
        if row.get("dir") == "server" and current.get("type") == "SessionList":
            known.update(session.get("session_id") for session in current.get("sessions", []))
        if row.get("dir") == "server" and current.get("type") == "WorkspaceTransferResult" and str(current.get("result", "")).lower() == "transferred":
            result = True

        tree = None
        if row.get("dir") == "client" and current.get("type") == "ReportWorkspaceTree":
            tree = current.get("tree")
        if row.get("dir") == "server" and current.get("type") == "SessionList":
            tree = current.get("workspace_tree")
        found = leaves(tree) if tree else []
        if source_workspace is not None and len(found) == 1 and str(found[0].get("workspace_id")) == source_workspace:
            source = True
        if len(found) == 1 and str(found[0].get("workspace_id")) == wanted:
            exact = json.dumps(found[0], sort_keys=True, separators=(",", ":")) == expected_leaf
            # Tear-out accepts the first exact target tree because target startup
            # can report follow-up attachment state; palette retains its final-tree
            # assertion after its own atomic transfer.
            target = target or exact if source_workspace is not None else exact

    assert result and target and (source_workspace is None or source), (result, source, target)
    assert creates == 0, creates
    assert created <= (known | source_sessions), created - known - source_sessions
    output = {
        "result": "Transferred",
        "target_leaf_exact": True,
        "create_session_frames": 0,
        "session_created_ids_preexisting": True,
    }
    if source_workspace is not None:
        output.update(target_window_id=transfer_target, source_collapsed=True)
    print(json.dumps(output, sort_keys=True))
    return 0


def frame_types(path, wanted_direction):
    for row in rows(path):
        if row.get("dir") == wanted_direction:
            print(message(row).get("type"))
    return 0


def reported_tree_leaves(node):
    if not isinstance(node, dict):
        return []
    if "Leaf" in node:
        return [node["Leaf"]]
    if "Split" in node:
        split = node["Split"]
        return reported_tree_leaves(split.get("first")) + reported_tree_leaves(split.get("second"))
    return []


def reported_leaves(path, wanted):
    tree = None
    for row in rows(path):
        current = message(row)
        if row.get("dir") == "client" and current.get("type") == "ReportWorkspaceTree":
            tree = current.get("tree")
    if tree is None:
        print("no client ReportWorkspaceTree frame recorded", file=sys.stderr)
        return 1
    found = reported_tree_leaves(tree)
    print(
        f"reported tree carries {len(found)} workspace leaves: "
        + ", ".join(str(leaf.get("workspace_id")) for leaf in found)
    )
    if len(found) != wanted:
        print(f"expected {wanted} leaves", file=sys.stderr)
        return 1
    return 0


def main(argv):
    path, command = argv[1:3]
    if command == "wait-leaves":
        return wait_leaves(path, int(argv[3]), int(argv[4]) if len(argv) > 4 else 0)
    if command == "wait-root":
        return wait_root(path, *argv[3:6])
    if command == "wait-report":
        return wait_report(path)
    if command == "count":
        print(count(path, argv[3]))
        return 0
    if command == "counts":
        print(json.dumps(counts(path), sort_keys=True))
        return 0
    if command == "summary":
        print(json.dumps({"counts": counts(path), "latest_tree": latest_tree(path)}, sort_keys=True))
        return 0
    if command == "leaf":
        return leaf(path, argv[3])
    if command == "leaf-session-count":
        return leaf_session_count(path, argv[3])
    if command == "assert-transfer":
        return assert_transfer(path, argv[3], argv[4], argv[5] if len(argv) > 5 else None)
    if command == "frame-types":
        return frame_types(path, argv[3])
    if command == "reported-leaves":
        return reported_leaves(path, int(argv[3]))
    raise SystemExit(f"unknown workspace oracle command: {command}")


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
