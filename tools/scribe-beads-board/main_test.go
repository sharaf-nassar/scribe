package main

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/steveyegge/beads/internal/storage/schema"
	"github.com/steveyegge/beads/internal/types"
	"github.com/steveyegge/beads/internal/workspacegate"
)

// @lat: [[test#Beads Board Reader Spike#Queue partition is exclusive and bounded]]
func TestPartitionIsExclusiveAndBounded(t *testing.T) {
	now := time.Now().UTC()
	issue := func(id string, status types.Status) *types.Issue {
		return &types.Issue{ID: id, Title: id, Status: status, UpdatedAt: now}
	}
	all := []*types.Issue{
		issue("done", types.StatusClosed),
		issue("blocked", types.StatusInProgress),
		issue("progress", types.StatusInProgress),
		issue("ready", types.StatusOpen),
		issue("backlog-1", types.StatusDeferred),
		issue("backlog-2", types.StatusOpen),
	}
	ready := []*types.Issue{all[1], all[2], all[3]}
	blocked := []*types.BlockedIssue{
		{Issue: *all[0], BlockedBy: []string{"old"}},
		{Issue: *all[1], BlockedBy: []string{"dependency"}},
	}

	got := partition(all, ready, blocked, 1)
	counts := []int{got.Backlog.Count, got.Ready.Count, got.InProgress.Count, got.Blocked.Count, got.Done.Count}
	want := []int{2, 1, 1, 1, 1}
	for i := range want {
		if counts[i] != want[i] {
			t.Fatalf("queue counts = %v, want %v", counts, want)
		}
	}
	for _, queue := range []queue{got.Backlog, got.Ready, got.InProgress, got.Blocked, got.Done} {
		if len(queue.Items) > 1 {
			t.Fatalf("queue returned %d items with limit 1", len(queue.Items))
		}
	}
	if got.Blocked.Items[0].ID != "blocked" || got.Blocked.Items[0].BlockedBy[0] != "dependency" {
		t.Fatalf("blocked item = %#v", got.Blocked.Items[0])
	}
	if got.Done.Items[0].ID != "done" {
		t.Fatalf("done precedence lost: %#v", got.Done.Items[0])
	}

	payload, err := json.Marshal(snapshot{FormatVersion: formatVersion, Queues: got})
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(payload), `"format_version":1`) || !strings.Contains(string(payload), `"in_progress"`) {
		t.Fatalf("contract fields missing from %s", payload)
	}
}

func TestPartitionTreatsStoredBlockedStatusAsBlocked(t *testing.T) {
	issue := &types.Issue{
		ID: "stored-blocked", Title: "stored-blocked",
		Status: types.StatusBlocked, UpdatedAt: time.Now().UTC(),
	}

	got := partition([]*types.Issue{issue}, nil, nil, 1)
	if got.Blocked.Count != 1 || len(got.Blocked.Items) != 1 {
		t.Fatalf("blocked queue = %#v, want stored blocked issue", got.Blocked)
	}
	if got.Backlog.Count != 0 {
		t.Fatalf("backlog count = %d, want 0", got.Backlog.Count)
	}
}

func TestSchemaVersionMustMatchExactly(t *testing.T) {
	latest := schema.LatestVersion()
	if err := validateSchemaVersion(latest); err != nil {
		t.Fatalf("matching schema: %v", err)
	}
	var behind *schema.SchemaBehindError
	if err := validateSchemaVersion(latest - 1); !errors.As(err, &behind) {
		t.Fatalf("older schema error = %T, want *schema.SchemaBehindError", err)
	}
	var ahead *schema.SchemaSkewError
	if err := validateSchemaVersion(latest + 1); !errors.As(err, &ahead) {
		t.Fatalf("newer schema error = %T, want *schema.SchemaSkewError", err)
	}
}

func TestSchemaVersionsIncludeIgnoredCursor(t *testing.T) {
	mainLatest := schema.LatestVersion()
	ignoredLatest := schema.LatestIgnoredVersion()
	if err := validateSchemaVersions(mainLatest, ignoredLatest); err != nil {
		t.Fatalf("matching schema cursors: %v", err)
	}
	for _, current := range []int{ignoredLatest - 1, ignoredLatest + 1} {
		var mismatch *ignoredSchemaVersionError
		if err := validateSchemaVersions(mainLatest, current); !errors.As(err, &mismatch) {
			t.Fatalf("ignored schema v%d error = %T, want *ignoredSchemaVersionError", current, err)
		}
		if mismatch.DBVersion != current || mismatch.BinaryVersion != ignoredLatest {
			t.Fatalf("ignored schema error = %#v", mismatch)
		}
	}
}

func TestReadGatesExcludeMaintenance(t *testing.T) {
	beadsDir := filepath.Join(t.TempDir(), ".beads")
	if err := os.MkdirAll(beadsDir, 0o700); err != nil {
		t.Fatal(err)
	}
	metadata := `{"database":"dolt","backend":"dolt","dolt_mode":"embedded","dolt_database":"test"}`
	if err := os.WriteFile(filepath.Join(beadsDir, "metadata.json"), []byte(metadata), 0o600); err != nil {
		t.Fatal(err)
	}

	handle, err := acquireReadGates(context.Background(), beadsDir)
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = handle.Release() }()

	for _, gate := range []workspacegate.Gate{
		mustWorkspaceGate(t, beadsDir),
		mustPhysicalGate(t, filepath.Join(beadsDir, "embeddeddolt")),
	} {
		other, err := gate.Acquire(context.Background(), workspacegate.Exclusive, workspacegate.Options{})
		if other != nil {
			_ = other.Release()
		}
		if !errors.Is(err, workspacegate.ErrBusy) {
			t.Fatalf("exclusive maintenance under board read = %v, want ErrBusy", err)
		}
	}
}

func mustWorkspaceGate(t *testing.T, beadsDir string) workspacegate.Gate {
	t.Helper()
	gate, err := workspacegate.ForWorkspace(beadsDir)
	if err != nil {
		t.Fatal(err)
	}
	return gate
}

func mustPhysicalGate(t *testing.T, root string) workspacegate.Gate {
	t.Helper()
	gate, err := workspacegate.ForPhysicalRoot(root)
	if err != nil {
		t.Fatal(err)
	}
	return gate
}
