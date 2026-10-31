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


_NO_TREE = object()


def workspace_tree(row):
    current = message(row)
    if row.get("dir") == "client" and current.get("type") == "ReportWorkspaceTree":
        return current.get("tree")
    if row.get("dir") == "server" and current.get("type") == "SessionList" and current.get("workspace_tree"):
        return current["workspace_tree"]
    return _NO_TREE


def trees(path):
    for row in rows(path):
        tree = workspace_tree(row)
        if tree is not _NO_TREE:
            yield tree


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


def wait_leaf_session_count(path, wanted, count):
    deadline = time.time() + 30
    while time.time() < deadline:
        for item in leaves(latest_tree(path)):
            if str(item.get("workspace_id")) == wanted and len(item.get("session_ids", [])) == count:
                print(json.dumps(item, sort_keys=True, separators=(",", ":")))
                return 0
        time.sleep(0.2)
    return 1


def assert_workspace_move(path, source_workspace, target_workspace, operation):
    request = moved = False
    created = set()
    known = set()
    lists = 0
    for row in rows(path):
        current = message(row)
        if row.get("dir") == "client" and current.get("type") == "MoveWorkspace":
            if (
                str(current.get("workspace_id")) == source_workspace
                and str(current.get("target_workspace_id")) == target_workspace
            ):
                found = current.get("operation", {})
                if operation == "swap":
                    request = found == "swap" or (isinstance(found, dict) and ("swap" in found or "Swap" in found))
                elif isinstance(found, dict):
                    edge = found.get("insert_at_edge", found.get("InsertAtEdge", {}))
                    request = edge.get("edge", "").lower() == operation
        if row.get("dir") == "server" and current.get("type") == "WorkspaceMoveResult":
            moved = moved or str(current.get("result", "")).lower() == "moved"
        if row.get("dir") == "server" and current.get("type") == "SessionList":
            lists += 1
            known.update(session.get("session_id") for session in current.get("sessions", []))
        if row.get("dir") == "server" and current.get("type") == "SessionCreated":
            created.add(current.get("session_id"))
    # A move itself creates no PTY. The destination client may attach an
    # already-listed arrival after its full refresh, which is distinguishable
    # from a replacement session because every such id was in SessionList.
    assert request and moved and lists >= 2 and created <= known, (
        request,
        moved,
        lists,
        created - known,
    )
    print(json.dumps({"operation": operation, "session_created_ids_preexisting": True, "session_lists": lists}))
    return 0


def assert_tab_subtree_move(path, source_workspace, target_workspace, session_id):
    request = moved = False
    lists = 0
    created = set()
    known = set()
    for row in rows(path):
        current = message(row)
        if row.get("dir") == "client" and current.get("type") == "MoveWorkspace":
            operation = current.get("operation", {}).get("move_tab", current.get("operation", {}).get("MoveTab", {}))
            request = request or (
                str(current.get("workspace_id")) == source_workspace
                and str(current.get("target_workspace_id")) == target_workspace
                and str(operation.get("tab_session_id")) == session_id
            )
        if row.get("dir") == "server" and current.get("type") == "WorkspaceMoveResult":
            moved = moved or str(current.get("result", "")).lower() == "moved"
        if row.get("dir") == "server" and current.get("type") == "SessionList":
            lists += 1
            known.update(session.get("session_id") for session in current.get("sessions", []))
        if row.get("dir") == "server" and current.get("type") == "SessionCreated":
            created.add(current.get("session_id"))
    # Same rule as assert_workspace_move: the client may re-attach the panes it
    # shows after adopting the refreshed tree, and the server answers each with
    # a SessionCreated for an id the one SessionList already named. A
    # replacement session would be an id that list never carried.
    assert request and moved and lists == 1 and created <= known, (
        request,
        moved,
        lists,
        created - known,
    )
    print(json.dumps({"session_id": session_id, "session_created_ids_preexisting": True, "session_lists": lists}))
    return 0


def latest_leaf(path, wanted):
    """The most recently reported leaf for `wanted`, from whichever window
    reported it. With several windows on one wire record, the newest tree
    overall belongs to whichever window spoke last, so a per-workspace lookup
    has to search every tree rather than only the latest."""
    found = None
    for tree in trees(path):
        for item in leaves(tree):
            if str(item.get("workspace_id")) == wanted:
                found = item
    return found


def wait_latest_leaf_count(path, wanted, count):
    deadline = time.time() + 30
    while time.time() < deadline:
        item = latest_leaf(path, wanted)
        if item is not None and len(item.get("session_ids", [])) == count:
            print(json.dumps(item, sort_keys=True, separators=(",", ":")))
            return 0
        time.sleep(0.2)
    return 1


def wait_tabs_together(path, anchor, moved):
    """Wait until the newest report of some workspace holds both `anchor` and
    `moved`: the drop landed in the window that already held `anchor`."""
    deadline = time.time() + 30
    while time.time() < deadline:
        newest = {}
        for tree in trees(path):
            for item in leaves(tree):
                newest[str(item.get("workspace_id"))] = item
        for item in newest.values():
            ids = [str(session) for session in item.get("session_ids", [])]
            if anchor in ids and moved in ids:
                print(json.dumps(item, sort_keys=True, separators=(",", ":")))
                return 0
        time.sleep(0.2)
    return 1


def known_workspaces(path):
    seen = []
    for tree in trees(path):
        for item in leaves(tree):
            workspace = str(item.get("workspace_id"))
            if workspace not in seen:
                seen.append(workspace)
    print(" ".join(seen))
    return 0


def _no_replacement_sessions(path):
    created = set()
    known = set()
    for row in rows(path):
        current = message(row)
        if row.get("dir") != "server":
            continue
        if current.get("type") == "SessionList":
            known.update(session.get("session_id") for session in current.get("sessions", []))
        if current.get("type") == "SessionCreated":
            created.add(current.get("session_id"))
    return created - known


def assert_cross_window_tab_move(path, source_workspace, target_workspace, session_id, source_closed=False):
    request = moved = closed = False
    lists = 0
    for row in rows(path):
        current = message(row)
        if row.get("dir") == "client" and current.get("type") == "MoveWorkspace":
            operation = current.get("operation", {}).get("move_tab", {})
            request = request or (
                str(current.get("workspace_id")) == source_workspace
                and str(current.get("target_workspace_id")) == target_workspace
                and str(operation.get("tab_session_id")) == session_id
            )
        if row.get("dir") == "server" and current.get("type") == "WorkspaceMoveResult":
            moved = moved or str(current.get("result", "")).lower() == "moved"
        if row.get("dir") == "server" and current.get("type") == "SessionList":
            lists += 1
        if row.get("dir") == "server" and current.get("type") == "WindowClosed":
            closed = True
    stray = _no_replacement_sessions(path)
    # Both windows' connections are refreshed, so at least two lists, unless
    # the move emptied the source: it is closed instead of refreshed.
    refreshed = (closed and lists >= 1) if source_closed else lists >= 2
    assert request and moved and refreshed and not stray, (request, moved, lists, closed, stray)
    print(json.dumps({"session_id": session_id, "session_lists": lists}))
    return 0


def assert_tab_transfer(path, session_id):
    request = transferred = False
    for row in rows(path):
        current = message(row)
        if row.get("dir") == "client" and current.get("type") == "TransferTab":
            request = request or str(current.get("tab_session_id")) == session_id
        if row.get("dir") == "server" and current.get("type") == "WorkspaceTransferResult":
            transferred = transferred or str(current.get("result", "")).lower() == "transferred"
    stray = _no_replacement_sessions(path)
    assert request and transferred and not stray, (request, transferred, stray)
    print(json.dumps({"session_id": session_id, "transferred": True}))
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

        found = leaves(workspace_tree(row))
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


def wait_frame(path, wanted_direction, wanted_type, needle, timeout):
    started = int(time.time())
    while True:
        for row in rows(path):
            current = message(row)
            if (
                row.get("dir") == wanted_direction
                and current.get("type") == wanted_type
                and (not needle or needle in json.dumps(current))
            ):
                print(json.dumps(current)[:400])
                return 0
        if int(time.time()) - started >= timeout:
            return 1
        time.sleep(0.3)


def first_frame_field(path, wanted_direction, wanted_type, field):
    for row in rows(path):
        current = message(row)
        if (
            row.get("dir") == wanted_direction
            and current.get("type") == wanted_type
            and field in current
        ):
            print(current[field])
            return 0
    return 1


def reported_leaves(path, wanted):
    tree = None
    for row in rows(path):
        current = message(row)
        if row.get("dir") == "client" and current.get("type") == "ReportWorkspaceTree":
            tree = current.get("tree")
    if tree is None:
        print("no client ReportWorkspaceTree frame recorded", file=sys.stderr)
        return 1
    found = leaves(tree)
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
        return wait_leaves(path, int(argv[3]), int(argv[4]))
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
    if command == "wait-leaf-session-count":
        return wait_leaf_session_count(path, argv[3], int(argv[4]))
    if command == "assert-workspace-move":
        return assert_workspace_move(path, argv[3], argv[4], argv[5])
    if command == "assert-tab-subtree-move":
        return assert_tab_subtree_move(path, argv[3], argv[4], argv[5])
    if command == "latest-leaf":
        item = latest_leaf(path, argv[3])
        if item is None:
            return 1
        print(json.dumps(item, sort_keys=True, separators=(",", ":")))
        return 0
    if command == "wait-latest-leaf-count":
        return wait_latest_leaf_count(path, argv[3], int(argv[4]))
    if command == "wait-tabs-together":
        return wait_tabs_together(path, argv[3], argv[4])
    if command == "known-workspaces":
        return known_workspaces(path)
    if command == "assert-cross-window-tab-move":
        return assert_cross_window_tab_move(
            path, argv[3], argv[4], argv[5], len(argv) > 6 and argv[6] == "source-closed"
        )
    if command == "assert-tab-transfer":
        return assert_tab_transfer(path, argv[3])
    if command == "assert-transfer":
        return assert_transfer(path, argv[3], argv[4], argv[5] if len(argv) > 5 else None)
    if command == "frame-types":
        return frame_types(path, argv[3])
    if command == "wait-frame":
        return wait_frame(path, argv[3], argv[4], argv[5], int(argv[6]))
    if command == "first-frame-field":
        return first_frame_field(path, argv[3], argv[4], argv[5])
    if command == "reported-leaves":
        return reported_leaves(path, int(argv[3]))
    raise SystemExit(f"unknown workspace oracle command: {command}")


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
