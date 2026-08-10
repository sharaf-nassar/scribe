package main

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"

	beadsdiscovery "github.com/steveyegge/beads/internal/beads"
	"github.com/steveyegge/beads/internal/configfile"
	"github.com/steveyegge/beads/internal/doltserver"
	"github.com/steveyegge/beads/internal/storage/embeddeddolt"
	storageops "github.com/steveyegge/beads/internal/storage/issueops"
	"github.com/steveyegge/beads/internal/storage/schema"
	"github.com/steveyegge/beads/internal/types"
	"github.com/steveyegge/beads/internal/workapi"
	"github.com/steveyegge/beads/internal/workspacegate"
	"github.com/steveyegge/beads/issueops"
)

const formatVersion = 1

type snapshot struct {
	FormatVersion int       `json:"format_version"`
	GeneratedAt   time.Time `json:"generated_at"`
	WorkspaceRoot string    `json:"workspace_root"`
	ItemLimit     int       `json:"item_limit"`
	Queues        queues    `json:"queues"`
}

type queues struct {
	Backlog    queue `json:"backlog"`
	Ready      queue `json:"ready"`
	InProgress queue `json:"in_progress"`
	Blocked    queue `json:"blocked"`
	Done       queue `json:"done"`
}

type queue struct {
	Count int         `json:"count"`
	Items []boardItem `json:"items"`
}

type boardItem struct {
	ID        string          `json:"id"`
	Title     string          `json:"title"`
	Status    types.Status    `json:"status"`
	Priority  int             `json:"priority"`
	IssueType types.IssueType `json:"issue_type,omitempty"`
	Assignee  string          `json:"assignee,omitempty"`
	UpdatedAt time.Time       `json:"updated_at"`
	BlockedBy []string        `json:"blocked_by,omitempty"`
}

func main() {
	directory := flag.String("directory", ".", "workspace directory")
	limit := flag.Int("limit", 8, "maximum items returned per queue (1-100)")
	flag.Parse()
	if *limit < 1 || *limit > 100 {
		fatalf("limit must be between 1 and 100")
	}

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	result, err := readSnapshot(ctx, *directory, *limit)
	if err != nil {
		fatalf("%v", err)
	}
	encoder := json.NewEncoder(os.Stdout)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(result); err != nil {
		fatalf("encode snapshot: %v", err)
	}
}

func fatalf(format string, args ...any) {
	fmt.Fprintf(os.Stderr, "scribe-beads-board: "+format+"\n", args...)
	os.Exit(1)
}

// @lat: [[client#Beads Board Reader Spike]]
func readSnapshot(ctx context.Context, directory string, limit int) (_ snapshot, err error) {
	beadsDir := beadsdiscovery.FindBeadsDirFrom(directory)
	if beadsDir == "" {
		return snapshot{}, fmt.Errorf("no Beads workspace found from %s", directory)
	}

	cfg, err := strictEmbeddedConfig(beadsDir)
	if err != nil {
		return snapshot{}, err
	}
	gates, err := acquireReadGates(ctx, beadsDir)
	if err != nil {
		return snapshot{}, fmt.Errorf("acquire Beads maintenance gates: %w", err)
	}
	defer func() { err = errors.Join(err, gates.Release()) }()

	dataDir := filepath.Join(beadsDir, "embeddeddolt")
	database := strings.NewReplacer("-", "_", ".", "_").Replace(cfg.GetDoltDatabase())
	db, cleanup, err := embeddeddolt.OpenSQL(ctx, dataDir, database, "main")
	if err != nil {
		return snapshot{}, fmt.Errorf("open embedded Dolt read-only: %w", err)
	}
	defer func() { err = errors.Join(err, cleanup()) }()

	currentVersion, err := schema.CurrentVersion(ctx, db)
	if err != nil {
		return snapshot{}, fmt.Errorf("read Beads schema version: %w", err)
	}
	currentIgnoredVersion, err := schema.CurrentIgnoredVersion(ctx, db)
	if err != nil {
		return snapshot{}, fmt.Errorf("read Beads ignored schema version: %w", err)
	}
	if err := validateSchemaVersions(currentVersion, currentIgnoredVersion); err != nil {
		return snapshot{}, err
	}

	tx, err := db.BeginTx(ctx, &sql.TxOptions{ReadOnly: true})
	if err != nil {
		return snapshot{}, fmt.Errorf("begin board snapshot: %w", err)
	}
	defer func() { _ = tx.Rollback() }()

	allFilter, err := workapi.BuildListFilter(issueops.ListRequest{
		Status:       "open,in_progress,blocked,deferred,closed",
		NoPinnedFlag: true,
		SkipLabels:   true,
		SkipCounts:   true,
		SortBy:       "priority",
		Limit:        intPtr(0),
	}, workapi.ListConfig{})
	if err != nil {
		return snapshot{}, fmt.Errorf("build Beads list filter: %w", err)
	}
	allFilter.Lite = true
	all, err := storageops.SearchIssuesInTx(ctx, tx, "", allFilter)
	if err != nil {
		return snapshot{}, fmt.Errorf("read Beads issues: %w", err)
	}

	readyFilter, err := workapi.BuildReadyFilter(issueops.ReadyRequest{
		Sort:  string(types.SortPolicyPriority),
		Limit: intPtr(0),
	})
	if err != nil {
		return snapshot{}, fmt.Errorf("build Beads ready filter: %w", err)
	}
	ready, err := storageops.GetReadyWorkInTx(ctx, tx, readyFilter)
	if err != nil {
		return snapshot{}, fmt.Errorf("read Beads ready issues: %w", err)
	}
	blocked, err := storageops.GetBlockedIssuesInTx(ctx, tx, types.WorkFilter{})
	if err != nil {
		return snapshot{}, fmt.Errorf("read Beads blocked issues: %w", err)
	}

	return snapshot{
		FormatVersion: formatVersion,
		GeneratedAt:   time.Now().UTC(),
		WorkspaceRoot: filepath.Dir(beadsDir),
		ItemLimit:     limit,
		Queues:        partition(all, ready, blocked, limit),
	}, nil
}

func strictEmbeddedConfig(beadsDir string) (*configfile.Config, error) {
	metadata := configfile.ConfigPath(beadsDir)
	if _, err := os.Stat(metadata); err != nil {
		return nil, fmt.Errorf("strict reader requires existing %s: %w", metadata, err)
	}
	cfg, err := configfile.Load(beadsDir)
	if err != nil {
		return nil, fmt.Errorf("load Beads metadata: %w", err)
	}
	if cfg == nil {
		return nil, fmt.Errorf("no Beads metadata in %s", metadata)
	}
	if !configfile.IsSupportedBackend(cfg.Backend) || cfg.GetBackend() != configfile.BackendDolt {
		return nil, fmt.Errorf("unsupported Beads backend %q", cfg.Backend)
	}
	if cfg.IsDoltProxiedServerMode() || cfg.IsDoltServerMode() || doltserver.IsSharedServerMode() {
		return nil, errors.New("spike supports embedded Dolt only")
	}
	return cfg, nil
}

func validateSchemaVersion(current int) error {
	latest := schema.LatestVersion()
	if current > latest {
		return &schema.SchemaSkewError{DBVersion: current, BinaryVersion: latest}
	}
	if current < latest {
		return &schema.SchemaBehindError{DBVersion: current, BinaryVersion: latest}
	}
	return nil
}

type ignoredSchemaVersionError struct {
	DBVersion     int
	BinaryVersion int
}

func (e *ignoredSchemaVersionError) Error() string {
	return fmt.Sprintf("ignored schema version mismatch: database is at v%d, binary expects v%d",
		e.DBVersion, e.BinaryVersion)
}

func validateSchemaVersions(current, ignored int) error {
	if err := validateSchemaVersion(current); err != nil {
		return err
	}
	latestIgnored := schema.LatestIgnoredVersion()
	if ignored != latestIgnored {
		return &ignoredSchemaVersionError{DBVersion: ignored, BinaryVersion: latestIgnored}
	}
	return nil
}

func acquireReadGates(ctx context.Context, beadsDir string) (*workspacegate.MultiHandle, error) {
	workspace, err := workspacegate.ForWorkspace(beadsDir)
	if err != nil {
		return nil, err
	}
	gates := []workspacegate.Gate{workspace}
	physical, err := doltserver.ResolvePhysicalRoots(beadsDir)
	if err != nil {
		return nil, err
	}
	for _, root := range physical.Roots {
		gate, err := workspacegate.ForPhysicalRoot(root)
		if err != nil {
			return nil, err
		}
		gates = append(gates, gate)
	}
	return workspacegate.AcquireAll(ctx, workspacegate.Shared,
		workspacegate.Options{Wait: 250 * time.Millisecond}, gates...)
}

func partition(all, ready []*types.Issue, blocked []*types.BlockedIssue, limit int) queues {
	result := queues{
		Backlog:    queue{Items: []boardItem{}},
		Ready:      queue{Items: []boardItem{}},
		InProgress: queue{Items: []boardItem{}},
		Blocked:    queue{Items: []boardItem{}},
		Done:       queue{Items: []boardItem{}},
	}
	readyIDs := make(map[string]bool, len(ready))
	for _, issue := range ready {
		readyIDs[issue.ID] = true
	}
	blockedBy := make(map[string][]string, len(blocked))
	for _, issue := range blocked {
		blockedBy[issue.ID] = issue.BlockedBy
	}

	for _, issue := range all {
		item := boardItem{
			ID: issue.ID, Title: issue.Title, Status: issue.Status,
			Priority: issue.Priority, IssueType: issue.IssueType,
			Assignee: issue.Assignee, UpdatedAt: issue.UpdatedAt,
		}
		blockers, isDerivedBlocked := blockedBy[issue.ID]
		var target *queue
		switch {
		case issue.Status == types.StatusClosed:
			target = &result.Done
		case issue.Status == types.StatusBlocked || isDerivedBlocked:
			item.BlockedBy = append([]string(nil), blockers...)
			target = &result.Blocked
		case issue.Status == types.StatusInProgress:
			target = &result.InProgress
		case readyIDs[issue.ID]:
			target = &result.Ready
		default:
			target = &result.Backlog
		}
		target.Count++
		if len(target.Items) < limit {
			target.Items = append(target.Items, item)
		}
	}
	return result
}

func intPtr(value int) *int { return &value }
