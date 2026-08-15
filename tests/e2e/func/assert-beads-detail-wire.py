#!/usr/bin/env python3
"""Compare the real bd record with the detail response painted by the client."""

import argparse
import json
from pathlib import Path


def issue_from_bd(payload):
    if not isinstance(payload, list) or len(payload) != 1:
        raise AssertionError("bd show did not return exactly one issue")
    return payload[0]


def latest_detail(record: Path, issue_id: str):
    found = None
    for line in record.read_text().splitlines():
        try:
            row = json.loads(line)
        except ValueError:
            continue
        message = row.get("message", {})
        if (
            row.get("dir") == "server"
            and message.get("type") == "BeadsIssueDetail"
            and message.get("issue_id") == issue_id
        ):
            found = message.get("detail")
    if found is None:
        raise AssertionError(f"wire record has no detail response for {issue_id}")
    return found


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bd", type=Path, required=True)
    parser.add_argument("--wire", type=Path, required=True)
    parser.add_argument("--issue", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    source = issue_from_bd(json.loads(args.bd.read_text()))
    painted = latest_detail(args.wire, args.issue)
    fields = [
        "id",
        "title",
        "description",
        "acceptance_criteria",
        "notes",
        "design",
        "spec_id",
        "status",
        "priority",
        "issue_type",
        "labels",
        "assignee",
        "created_at",
        "updated_at",
        "external_ref",
        "owner",
        "due_at",
        "estimated_minutes",
    ]
    compared = {}
    for field in fields:
        compared[field] = source.get(field)
        if painted.get(field) != source.get(field):
            raise AssertionError(
                f"painted {field} {painted.get(field)!r} != bd show {source.get(field)!r}"
            )
    dependencies = source.get("dependencies", [])
    parent = next(
        (item.get("title") for item in dependencies if item.get("dependency_type") == "parent-child"),
        None,
    )
    blockers = sorted(
        item.get("id", item.get("depends_on_id"))
        for item in dependencies
        if item.get("dependency_type") == "blocks"
    )
    blockers.extend(
        item if isinstance(item, str) else item.get("id") for item in source.get("blocked_by", [])
    )
    if painted.get("parent_epic_name") != parent:
        raise AssertionError(f"painted epic {painted.get('parent_epic_name')!r} != bd show {parent!r}")
    if sorted(item["id"] for item in painted.get("blockers", [])) != sorted(blockers):
        raise AssertionError("painted blockers differ from bd show")

    dependent_ids = sorted(
        item["id"]
        for item in source.get("dependents", [])
        if item.get("dependency_type", "blocks") == "blocks"
    )
    if sorted(item["id"] for item in painted.get("dependents", [])) != dependent_ids:
        raise AssertionError("painted dependents differ from bd show")
    comments = [
        {"author": item.get("author", ""), "created_at": item.get("created_at", ""), "body": item.get("text", "")}
        for item in reversed(source.get("comments", []))
    ]
    if painted.get("comments") != comments:
        raise AssertionError("painted comments differ from newest-first bd show")
    if painted.get("queue") != "blocked" or painted.get("queue_basis") != "open_blockers":
        raise AssertionError(
            f"painted queue {painted.get('queue')}/{painted.get('queue_basis')} is not blocker-derived"
        )

    evidence = {
        "issue": args.issue,
        "compared_fields": compared,
        "parent_epic_name": parent,
        "blocker_ids": sorted(blockers),
        "dependent_ids": dependent_ids,
        "comments": comments,
        "queue": painted["queue"],
        "queue_basis": painted["queue_basis"],
    }
    args.output.write_text(json.dumps(evidence, indent=2) + "\n")
    print(json.dumps(evidence, separators=(",", ":")))


if __name__ == "__main__":
    main()
