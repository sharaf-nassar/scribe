// Scribe Driver UI — full IPC bridge and SPA renderer
// No frameworks, no build tools, vanilla JS with tabs.
//
// innerHTML usage policy: Only three patterns are used:
//   1. container.innerHTML = '' — clearing a container (no content written)
//   2. pre.innerHTML = renderOutputHtml(buf) — content is fully escaped via
//      escapeHtml() before any HTML spans are added; user data never reaches
//      innerHTML un-escaped.
//   3. Static el() builder — builds DOM nodes via createElement; never uses
//      innerHTML for user-controlled strings.

// ---------------------------------------------------------------------------
// Debug stub (no console.log in production)
// ---------------------------------------------------------------------------

function debug() {}

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

let driverState = {
	tasks: [],
	stats: { running: 0, completed: 0, failed: 0, total_tokens: 0 },
	projects: [],
};

// Path to auto-select after a project_added response
let pendingSelectPath = null;
// Cleanup function for project dropdown listeners — set by renderNewTask(), called on view change
let cleanupProjectDropdown = null;
let currentView = 'dashboard';
let selectedTaskId = null;
let taskOutputBuffers = {};
let userScrolledUp = false;

// ---------------------------------------------------------------------------
// IPC helpers
// ---------------------------------------------------------------------------

function sendMessage(msg) {
	if (window.ipc && typeof window.ipc.postMessage === 'function') {
		window.ipc.postMessage(JSON.stringify(msg));
	}
}

// ---------------------------------------------------------------------------
// Project color assignment (FNV-1a hash)
// ---------------------------------------------------------------------------

const PROJECT_COLORS = [
	'#7c3aed',
	'#2563eb',
	'#f43f5e',
	'#059669',
	'#d946ef',
	'#f59e0b',
	'#06b6d4',
	'#84cc16',
];

function projectColor(path) {
	let hash = 2166136261;
	for (let i = 0; i < path.length; i++) {
		hash ^= path.charCodeAt(i);
		hash = Math.imul(hash, 16777619);
	}
	return PROJECT_COLORS[Math.abs(hash) % PROJECT_COLORS.length];
}

// ---------------------------------------------------------------------------
// Task state helpers
// ---------------------------------------------------------------------------

const ACTIVE_STATES = new Set(['Running', 'Starting', 'WaitingForInput', 'PermissionPrompt']);
const DONE_STATES = new Set(['Completed', 'Failed', 'Stopped']);

function isActive(task) {
	return ACTIVE_STATES.has(task.state);
}

function isDone(task) {
	return DONE_STATES.has(task.state);
}

function projectName(path) {
	if (!path) return '(unknown)';
	const parts = path.replace(/\/$/, '').split('/');
	return parts[parts.length - 1] || path;
}

// ---------------------------------------------------------------------------
// Time formatting
// ---------------------------------------------------------------------------

function formatElapsed(createdAt) {
	const now = Math.floor(Date.now() / 1000);
	const diff = Math.max(0, now - createdAt);
	if (diff < 60) return diff + 's';
	const mins = Math.floor(diff / 60);
	const secs = diff % 60;
	if (mins < 60) return mins + 'm ' + secs + 's';
	const hours = Math.floor(mins / 60);
	const remMins = mins % 60;
	return hours + 'h ' + remMins + 'm';
}

function formatTimeAgo(timestamp) {
	const now = Math.floor(Date.now() / 1000);
	const diff = Math.max(0, now - timestamp);
	if (diff < 60) return diff + 's ago';
	const mins = Math.floor(diff / 60);
	if (mins < 60) return mins + 'm ago';
	const hours = Math.floor(mins / 60);
	if (hours < 24) return hours + 'h ago';
	const days = Math.floor(hours / 24);
	return days + 'd ago';
}

// ---------------------------------------------------------------------------
// Output line formatter
//
// escapeHtml() is applied FIRST so user data is always sanitized.
// The <span> wrappers added after escaping are trusted static strings.
// ---------------------------------------------------------------------------

function escapeHtml(text) {
	return text
		.replace(/&/g, '&amp;')
		.replace(/</g, '&lt;')
		.replace(/>/g, '&gt;')
		.replace(/"/g, '&quot;');
}

function formatOutputLine(line) {
	// Escape user content before any HTML is added
	const safe = escapeHtml(line);

	// Wave headers
	if (/^▶\s+Wave\s+\d+/u.test(line)) {
		return '<span class="output-wave">' + safe + '</span>';
	}
	// Checkmarks
	if (/^✓/.test(line)) {
		return '<span class="output-ok">' + safe + '</span>';
	}
	// Error lines
	if (/^(error|Error|ERROR)/.test(line)) {
		return '<span class="output-error">' + safe + '</span>';
	}
	// Prefixed tool lines
	if (/^Read\s+/.test(line)) {
		return '<span class="output-read">' + safe + '</span>';
	}
	if (/^Write\s+/.test(line)) {
		return '<span class="output-write">' + safe + '</span>';
	}
	if (/^Run\s+/.test(line) || /^Bash\s+/.test(line)) {
		return '<span class="output-run">' + safe + '</span>';
	}
	if (/^Analyze\s+/.test(line) || /^Search\s+/.test(line) || /^Grep\s+/.test(line)) {
		return '<span class="output-analyze">' + safe + '</span>';
	}
	if (/^Verify\s+/.test(line) || /^Test\s+/.test(line)) {
		return '<span class="output-verify">' + safe + '</span>';
	}

	return safe;
}

function renderOutputHtml(text) {
	if (!text) return '';
	const lines = text.split('\n');
	return lines.map(formatOutputLine).join('\n');
}

// ---------------------------------------------------------------------------
// DOM builder helpers — creates elements without touching innerHTML
// ---------------------------------------------------------------------------

function el(tag, attrs, ...children) {
	const node = document.createElement(tag);
	if (attrs) {
		for (const [k, v] of Object.entries(attrs)) {
			if (k === 'class') {
				node.className = v;
			} else if (k === 'style') {
				node.style.cssText = v;
			} else if (k.startsWith('on') && typeof v === 'function') {
				node.addEventListener(k.slice(2), v);
			} else {
				node.setAttribute(k, v);
			}
		}
	}
	for (const child of children) {
		if (child == null) continue;
		if (typeof child === 'string') {
			node.appendChild(document.createTextNode(child));
		} else {
			node.appendChild(child);
		}
	}
	return node;
}

function setText(node, text) {
	node.textContent = text;
}

// ---------------------------------------------------------------------------
// Status dot → CSS class
// ---------------------------------------------------------------------------

function stateDotClass(state) {
	switch (state) {
		case 'Running': return 'status-dot--running';
		case 'Starting': return 'status-dot--running';
		case 'WaitingForInput': return 'status-dot--planning';
		case 'PermissionPrompt': return 'status-dot--planning';
		case 'Completed': return 'status-dot--done';
		case 'Failed': return 'status-dot--error';
		case 'Stopped': return 'status-dot--done';
		default: return '';
	}
}

function aiStateBorderClass(aiState) {
	if (!aiState) return '';
	switch (aiState) {
		case 'Processing': return 'ai-processing';
		case 'Waiting':
		case 'WaitingForInput': return 'ai-waiting';
		case 'PermissionPrompt': return 'ai-permission';
		case 'Error': return 'ai-error';
		default: return '';
	}
}

// ---------------------------------------------------------------------------
// Render: Sidebar
// ---------------------------------------------------------------------------

function renderSidebar() {
	const taskList = document.getElementById('sidebar-task-list');
	const statsRunning = document.getElementById('stat-running');
	const statsDone = document.getElementById('stat-done');
	const statsFail = document.getElementById('stat-fail');
	const statsTokens = document.getElementById('stat-tokens');

	if (!taskList) return;

	// Stats
	if (statsRunning) setText(statsRunning, String(driverState.stats.running));
	if (statsDone) setText(statsDone, String(driverState.stats.completed));
	if (statsFail) setText(statsFail, String(driverState.stats.failed));
	if (statsTokens) {
		const t = driverState.stats.total_tokens;
		setText(statsTokens, t >= 1000 ? Math.round(t / 1000) + 'k' : String(t));
	}

	// Task list — clear via removeChild loop (avoids innerHTML)
	while (taskList.firstChild) {
		taskList.removeChild(taskList.firstChild);
	}

	const tasks = driverState.tasks.slice();
	tasks.sort((a, b) => b.created_at - a.created_at);

	if (tasks.length === 0) {
		const empty = el('div', { class: 'sidebar-empty' }, 'No tasks yet');
		taskList.appendChild(empty);
		return;
	}

	for (const task of tasks) {
		const color = projectColor(task.project_path);
		const isSelected = task.id === selectedTaskId;
		const aiClass = aiStateBorderClass(task.ai_state);
		const classes = ['sidebar-task-entry'];
		if (isSelected) classes.push('selected');
		if (aiClass) classes.push(aiClass);

		const badge = el('span', {
			class: 'task-entry-badge',
			style: 'background:' + color + ';border-color:' + color,
		});

		const nameEl = el('span', { class: 'task-entry-name' });
		setText(nameEl, task.description || '(no description)');

		const projEl = el('span', { class: 'task-entry-project' });
		setText(projEl, projectName(task.project_path));

		const dotClass = stateDotClass(task.state);
		const dot = el('span', { class: 'status-dot' + (dotClass ? ' ' + dotClass : '') });

		const entry = el(
			'div',
			{
				class: classes.join(' '),
				onclick: () => showView('task-detail', task.id),
			},
			badge,
			el('div', { class: 'task-entry-info' }, nameEl, projEl),
			dot,
		);
		taskList.appendChild(entry);
	}
}

// ---------------------------------------------------------------------------
// Render: Dashboard
// ---------------------------------------------------------------------------

function renderDashboard() {
	const container = document.getElementById('view-dashboard');
	if (!container) return;

	// Clear container via removeChild
	while (container.firstChild) {
		container.removeChild(container.firstChild);
	}

	const activeTasks = driverState.tasks.filter(isActive);
	const doneTasks = driverState.tasks.filter(isDone);

	// Active agents section
	const activeSection = el('section', { class: 'section-group' });
	const activeHeader = el('div', { class: 'section-label' }, 'Active Agents');
	activeSection.appendChild(activeHeader);

	const activeCard = el('div', { class: 'section-card agent-card-list' });
	if (activeTasks.length === 0) {
		const empty = el('div', { class: 'empty-state' },
			el('div', { class: 'empty-icon' }, '🤖'),
			el('div', { class: 'empty-text' }, 'No active tasks'),
			el('button', {
				class: 'btn-primary',
				onclick: () => showView('new-task'),
			}, '+ New Task'),
		);
		activeCard.appendChild(empty);
	} else {
		for (const task of activeTasks) {
			activeCard.appendChild(buildTaskCard(task));
		}
	}
	activeSection.appendChild(activeCard);
	container.appendChild(activeSection);

	// Recently completed section
	if (doneTasks.length > 0) {
		const doneSection = el('section', { class: 'section-group' });
		const doneHeader = el('div', { class: 'section-label' }, 'Recently Completed');
		doneSection.appendChild(doneHeader);

		const doneCard = el('div', { class: 'section-card' });
		const recent = doneTasks
			.slice()
			.sort((a, b) => (b.completed_at || b.created_at) - (a.completed_at || a.created_at))
			.slice(0, 20);
		for (const task of recent) {
			doneCard.appendChild(buildDoneTaskRow(task));
		}
		doneSection.appendChild(doneCard);
		container.appendChild(doneSection);
	}
}

function buildTaskCard(task) {
	const color = projectColor(task.project_path);

	const agentBadge = el('div', {
		class: 'agent-badge',
		style: '--project-color:' + color,
	}, '🤖');

	const nameEl = el('div', { class: 'agent-task-name' });
	setText(nameEl, task.description || '(no description)');

	const projEl = el('div', { class: 'agent-project-row' });
	setText(projEl, projectName(task.project_path));

	const dotClass = stateDotClass(task.state);
	const dot = el('span', { class: 'status-dot' + (dotClass ? ' ' + dotClass : '') });

	const elapsed = el('span', { class: 'agent-elapsed' });
	setText(elapsed, formatElapsed(task.created_at));
	elapsed.dataset.createdAt = String(task.created_at);

	const statusRow = el('div', { class: 'agent-status-row' }, dot, elapsed);

	const progressBar = el('div', { class: 'progress-bar' },
		el('div', {
			class: 'progress-fill',
			style: 'width:' + taskProgressPercent(task) + '%;background:' + color,
		}),
	);

	const tokens = (task.metrics && task.metrics.tokens_used) || 0;
	const files = (task.metrics && task.metrics.files_changed) || 0;
	const tokensEl = el('span', { class: 'agent-tokens' });
	setText(tokensEl, tokens >= 1000 ? Math.round(tokens / 1000) + 'k tokens' : tokens + ' tokens');
	const filesEl = el('span', { class: 'agent-files-changed' });
	setText(filesEl, files + ' files');
	const metrics = el('div', { class: 'agent-card-bottom' }, tokensEl, filesEl);

	return el('div', {
		class: 'agent-card',
		onclick: () => showView('task-detail', task.id),
	},
		el('div', { class: 'agent-card-top' },
			agentBadge,
			el('div', { class: 'agent-card-info' },
				nameEl,
				projEl,
				statusRow,
			),
		),
		progressBar,
		metrics,
	);
}

function buildDoneTaskRow(task) {
	const color = projectColor(task.project_path);
	const dotClass = stateDotClass(task.state);
	const dot = el('span', { class: 'status-dot' + (dotClass ? ' ' + dotClass : '') });

	const nameEl = el('span', { class: 'completed-name' });
	setText(nameEl, task.description || '(no description)');

	const projEl = el('span', { class: 'completed-project' });
	setText(projEl, projectName(task.project_path));

	const timeEl = el('span', { class: 'completed-time' });
	setText(timeEl, formatTimeAgo(task.completed_at || task.created_at));

	const colorDot = el('span', {
		class: 'completed-project-dot',
		style: 'background:' + color,
	});

	return el('div', {
		class: 'completed-row',
		onclick: () => showView('task-detail', task.id),
	},
		colorDot,
		dot,
		nameEl,
		projEl,
		timeEl,
	);
}

function taskProgressPercent(task) {
	switch (task.state) {
		case 'Starting': return 5;
		case 'Running': return 50;
		case 'WaitingForInput': return 70;
		case 'PermissionPrompt': return 75;
		case 'Completed': return 100;
		case 'Failed':
		case 'Stopped': return 100;
		default: return 0;
	}
}

// ---------------------------------------------------------------------------
// Render: Task Detail
// ---------------------------------------------------------------------------

function renderTaskDetail() {
	const container = document.getElementById('view-task-detail');
	if (!container) return;

	// Clear container
	while (container.firstChild) {
		container.removeChild(container.firstChild);
	}
	userScrolledUp = false;

	if (!selectedTaskId) {
		container.appendChild(el('div', { class: 'empty-state' },
			el('div', { class: 'empty-icon' }, '🤖'),
			el('div', { class: 'empty-text' }, 'No task selected.'),
		));
		return;
	}

	const task = driverState.tasks.find(t => t.id === selectedTaskId);
	if (!task) {
		container.appendChild(el('div', { class: 'empty-state' },
			el('div', { class: 'empty-icon' }, '🤖'),
			el('div', { class: 'empty-text' }, 'Task not found.'),
		));
		return;
	}

	const color = projectColor(task.project_path);

	// Determine status badge modifier
	let statusBadgeClass = 'task-status-badge';
	if (task.state === 'WaitingForInput' || task.state === 'PermissionPrompt') {
		statusBadgeClass += ' status-planning';
	} else if (task.state === 'Completed' || task.state === 'Stopped') {
		statusBadgeClass += ' status-done';
	} else if (task.state === 'Failed') {
		statusBadgeClass += ' status-error';
	}

	// Header
	const agentBadge = el('div', {
		class: 'agent-badge agent-badge--large',
		style: '--project-color:' + color,
	}, '🤖');

	const taskNameEl = el('div', { class: 'task-detail-name' });
	setText(taskNameEl, task.description || '(no description)');

	const projEl = el('div', { class: 'task-detail-project' });
	setText(projEl, task.project_path);

	const dotClass = stateDotClass(task.state);
	const stateDot = el('span', { class: 'status-dot' + (dotClass ? ' ' + dotClass : '') });
	const stateText = document.createTextNode(' ' + task.state);
	const stateBadge = el('span', { class: statusBadgeClass }, stateDot, stateText);

	const elapsed = el('span', { class: 'task-elapsed' });
	setText(elapsed, formatElapsed(task.created_at));
	elapsed.dataset.createdAt = String(task.created_at);

	const header = el('div', { class: 'task-detail-header' },
		el('div', { class: 'task-detail-header-left' },
			agentBadge,
			el('div', { class: 'task-detail-title-block' },
				taskNameEl,
				projEl,
			),
		),
		el('div', { class: 'task-detail-header-right' }, stateBadge, elapsed),
	);

	// Metrics strip
	const taskMetrics = task.metrics || {};
	const tokens = taskMetrics.tokens_used || 0;
	const files = taskMetrics.files_changed || 0;
	const cost = taskMetrics.cost_usd || 0;
	const waves = taskMetrics.waves_completed || 0;

	const tokensEl = el('div', { class: 'metric-item' },
		el('span', { class: 'metric-label' }, 'Tokens'),
		el('span', { class: 'metric-value' }, tokens >= 1000 ? Math.round(tokens / 1000) + 'k' : String(tokens)),
	);
	const filesEl = el('div', { class: 'metric-item' },
		el('span', { class: 'metric-label' }, 'Files'),
		el('span', { class: 'metric-value' }, String(files)),
	);
	const costEl = el('div', { class: 'metric-item' },
		el('span', { class: 'metric-label' }, 'Cost'),
		el('span', { class: 'metric-value' }, '$' + cost.toFixed(4)),
	);
	const wavesEl = el('div', { class: 'metric-item' },
		el('span', { class: 'metric-label' }, 'Wave'),
		el('span', { class: 'metric-value' }, String(waves)),
	);

	const stopBtn = el('button', {
		class: 'stop-btn',
		onclick: () => {
			sendMessage({ type: 'StopTask', task_id: selectedTaskId });
		},
	}, 'Stop');

	if (!isActive(task)) {
		stopBtn.disabled = true;
		stopBtn.classList.add('stop-btn--disabled');
	}

	const metricsStrip = el('div', { class: 'task-metrics-strip' },
		tokensEl, filesEl, costEl, wavesEl,
		el('div', { class: 'metric-spacer' }),
		stopBtn,
	);

	// Output area — uses innerHTML for pre-escaped output HTML
	const outputArea = el('div', { class: 'task-output', id: 'output-area' });
	const outputContent = el('pre', { id: 'output-content' });
	const buf = taskOutputBuffers[selectedTaskId] || '';
	// Safe: renderOutputHtml escapes all user content via escapeHtml() before
	// adding any HTML spans. The result is trusted markup, not raw user data.
	outputContent.innerHTML = renderOutputHtml(buf);
	outputArea.appendChild(outputContent);

	// Track scroll position for auto-scroll control
	outputArea.addEventListener('scroll', () => {
		const atBottom = outputArea.scrollHeight - outputArea.scrollTop - outputArea.clientHeight < 40;
		userScrolledUp = !atBottom;
	});

	// Terminal prompt
	const promptColor = isActive(task) ? color : '#666';
	const promptSymbol = el('span', {
		class: 'prompt-char',
		style: 'color:' + promptColor,
	}, '❯');

	const promptInput = el('input', {
		type: 'text',
		class: 'prompt-input',
		id: 'prompt-input',
		placeholder: isActive(task) ? 'Send input to task...' : 'Task is not active',
	});

	if (!isActive(task)) {
		promptInput.disabled = true;
	}

	const promptRow = el('div', { class: 'prompt-input-row' }, promptSymbol, promptInput);

	container.appendChild(header);
	container.appendChild(metricsStrip);
	container.appendChild(outputArea);
	container.appendChild(promptRow);

	setupPromptInput();

	// Scroll to bottom on initial render
	outputArea.scrollTop = outputArea.scrollHeight;
}

// ---------------------------------------------------------------------------
// Render: New Task
// ---------------------------------------------------------------------------

function renderNewTask() {
	const container = document.getElementById('view-new-task');
	if (!container) return;

	// Clean up previous dropdown listeners if any
	if (cleanupProjectDropdown) {
		cleanupProjectDropdown();
		cleanupProjectDropdown = null;
	}

	// Clear container
	while (container.firstChild) {
		container.removeChild(container.firstChild);
	}

	// Dropdown closure state
	let selectedPath = null;
	let addMode = false;
	let popoverOpen = false;

	// Honour pending auto-select after project_added
	if (pendingSelectPath !== null) {
		const exists = driverState.projects.some(p => p.path === pendingSelectPath);
		if (exists) {
			selectedPath = pendingSelectPath;
		}
		pendingSelectPath = null;
	}

	// Error message element (shared)
	const errorMsg = el('div', { class: 'form-error', id: 'form-error' });
	errorMsg.style.display = 'none';

	// Submit button (created early so we can update disabled state)
	const submitBtn = el('button', {
		class: 'btn-launch',
		type: 'button',
	}, '🚀 Launch');

	function updateSubmitBtn() {
		const disabled = selectedPath === null;
		submitBtn.disabled = disabled;
		if (disabled) {
			submitBtn.classList.add('btn-disabled');
		} else {
			submitBtn.classList.remove('btn-disabled');
		}
	}

	// ── Dropdown mount point ──
	const dropdownMount = el('div', { class: 'project-dropdown' });

	function closePopover() {
		popoverOpen = false;
		rebuildDropdown();
	}

	// Outside-click and ESC listeners — stored so we can remove them
	let outsideClickHandler = null;
	let escKeyHandler = null;

	function attachPopoverListeners() {
		outsideClickHandler = (e) => {
			if (!dropdownMount.contains(e.target)) {
				closePopover();
			}
		};
		escKeyHandler = (e) => {
			if (e.key === 'Escape') {
				closePopover();
			}
		};
		document.addEventListener('mousedown', outsideClickHandler);
		document.addEventListener('keydown', escKeyHandler);
	}

	function detachPopoverListeners() {
		if (outsideClickHandler) {
			document.removeEventListener('mousedown', outsideClickHandler);
			outsideClickHandler = null;
		}
		if (escKeyHandler) {
			document.removeEventListener('keydown', escKeyHandler);
			escKeyHandler = null;
		}
	}

	// Register module-level cleanup so showView can detach listeners
	cleanupProjectDropdown = detachPopoverListeners;

	function rebuildDropdown() {
		// Clear mount
		while (dropdownMount.firstChild) {
			dropdownMount.removeChild(dropdownMount.firstChild);
		}

		if (addMode) {
			// Add mode: inline input row
			const pathInput = el('input', {
				type: 'text',
				class: 'project-add-input',
				placeholder: '~/work/my-project',
				autocomplete: 'off',
				spellcheck: 'false',
			});

			function confirmAdd() {
				const val = pathInput.value.trim();
				if (!val) {
					pathInput.focus();
					return;
				}
				pendingSelectPath = val;
				addMode = false;
				sendMessage({ type: 'AddProject', path: val });
				// rebuildDropdown will be called by receiveDriverMessage after response
				// but also rebuild now to get back to select mode immediately
				rebuildDropdown();
				updateSubmitBtn();
			}

			pathInput.addEventListener('keydown', (e) => {
				if (e.key === 'Enter') {
					e.preventDefault();
					confirmAdd();
				} else if (e.key === 'Escape') {
					addMode = false;
					rebuildDropdown();
					updateSubmitBtn();
				}
			});

			const confirmBtn = el('button', {
				class: 'project-add-confirm',
				type: 'button',
				title: 'Add project',
				onclick: confirmAdd,
			}, '✓');

			const cancelBtn = el('button', {
				class: 'project-add-cancel',
				type: 'button',
				title: 'Cancel',
				onclick: () => {
					addMode = false;
					rebuildDropdown();
					updateSubmitBtn();
				},
			}, '×');

			const addRow = el('div', { class: 'project-add-row' }, pathInput, confirmBtn, cancelBtn);
			dropdownMount.appendChild(addRow);

			// Focus the input after appending
			setTimeout(() => pathInput.focus(), 0);

		} else {
			// Select mode: trigger button
			const triggerClasses = ['project-trigger'];
			if (popoverOpen) triggerClasses.push('open');

			let triggerContent;
			if (selectedPath !== null) {
				const proj = driverState.projects.find(p => p.path === selectedPath);
				const displayName = proj ? proj.name : projectName(selectedPath);
				const dot = el('span', {
					class: 'project-option-dot',
					style: 'background:' + projectColor(selectedPath),
				});
				const nameEl = el('span', { class: 'project-option-name' });
				setText(nameEl, displayName);
				triggerContent = el('span', { class: 'project-trigger-selected' }, dot, nameEl);
			} else {
				const placeholder = el('span', { class: 'project-placeholder' }, 'Select a project...');
				triggerContent = placeholder;
			}

			const chevron = el('span', { class: 'project-trigger-chevron' }, '▾');

			const trigger = el('div', {
				class: triggerClasses.join(' '),
				role: 'button',
				tabindex: '0',
				onclick: () => {
					popoverOpen = !popoverOpen;
					if (popoverOpen) {
						attachPopoverListeners();
					} else {
						detachPopoverListeners();
					}
					rebuildDropdown();
				},
				onkeydown: (e) => {
					if (e.key === 'Enter' || e.key === ' ') {
						e.preventDefault();
						popoverOpen = !popoverOpen;
						if (popoverOpen) {
							attachPopoverListeners();
						} else {
							detachPopoverListeners();
						}
						rebuildDropdown();
					} else if (e.key === 'Escape' && popoverOpen) {
						popoverOpen = false;
						detachPopoverListeners();
						rebuildDropdown();
					}
				},
			}, triggerContent, chevron);

			dropdownMount.appendChild(trigger);

			if (popoverOpen) {
				const popover = el('div', { class: 'project-popover' });

				for (const proj of driverState.projects) {
					const dot = el('span', {
						class: 'project-option-dot',
						style: 'background:' + projectColor(proj.path),
					});
					const nameEl = el('span', { class: 'project-option-name' });
					setText(nameEl, proj.name);
					const pathEl = el('span', { class: 'project-option-path' });
					setText(pathEl, proj.path);

					const removeBtn = el('button', {
						class: 'project-option-remove',
						type: 'button',
						title: 'Remove project',
						onclick: (e) => {
							e.stopPropagation();
							if (selectedPath === proj.path) {
								selectedPath = null;
								updateSubmitBtn();
							}
							sendMessage({ type: 'RemoveProject', path: proj.path });
							popoverOpen = false;
							detachPopoverListeners();
						},
					}, '×');

					const option = el('div', {
						class: 'project-option',
						onclick: () => {
							selectedPath = proj.path;
							popoverOpen = false;
							detachPopoverListeners();
							rebuildDropdown();
							updateSubmitBtn();
						},
					}, dot, el('div', { class: 'project-option-text' }, nameEl, pathEl), removeBtn);

					popover.appendChild(option);
				}

				// "Add new project..." option
				const addOption = el('div', {
					class: 'project-option project-option-add',
					onclick: () => {
						addMode = true;
						popoverOpen = false;
						detachPopoverListeners();
						rebuildDropdown();
						updateSubmitBtn();
					},
				}, '＋ Add new project...');
				popover.appendChild(addOption);

				dropdownMount.appendChild(popover);
			}
		}
	}

	// Initial render of dropdown
	rebuildDropdown();
	updateSubmitBtn();

	// ── Description ──
	const descInput = el('textarea', {
		class: 'form-textarea',
		id: 'input-task-description',
		rows: '6',
		placeholder: 'Describe what you want the agent to do…',
		spellcheck: 'false',
	});

	// ── Advanced options toggle ──
	const chevronSvg = el('svg', {
		class: 'advanced-chevron',
		id: 'advanced-chevron',
		viewBox: '0 0 12 12',
		fill: 'none',
		stroke: 'currentColor',
		'stroke-width': '1.5',
	});
	const chevronPath = document.createElementNS('http://www.w3.org/2000/svg', 'path');
	chevronPath.setAttribute('d', 'M3 4.5l3 3 3-3');
	chevronSvg.appendChild(chevronPath);

	const advancedToggle = el('button', {
		class: 'advanced-toggle',
		id: 'btn-advanced-toggle',
		type: 'button',
		'aria-expanded': 'false',
	}, chevronSvg, 'Advanced options');

	const advancedPanel = el('div', {
		class: 'advanced-panel',
		id: 'advanced-panel',
		hidden: 'true',
	},
		el('div', { class: 'form-group form-group--nested' },
			el('label', { class: 'form-label', for: 'input-branch-name' }, 'Branch Name'),
			el('input', {
				type: 'text',
				id: 'input-branch-name',
				class: 'form-input form-input--mono',
				placeholder: 'auto-generated',
				autocomplete: 'off',
				spellcheck: 'false',
			}),
			el('div', { class: 'form-hint' }, 'Leave blank to auto-generate from task description'),
		),
		el('div', { class: 'form-group form-group--nested' },
			el('label', { class: 'form-label', for: 'input-base-branch' }, 'Base Branch'),
			el('input', {
				type: 'text',
				id: 'input-base-branch',
				class: 'form-input form-input--mono',
				placeholder: 'main',
				autocomplete: 'off',
				spellcheck: 'false',
			}),
		),
	);

	advancedToggle.addEventListener('click', () => {
		const expanded = advancedToggle.getAttribute('aria-expanded') === 'true';
		advancedToggle.setAttribute('aria-expanded', String(!expanded));
		if (expanded) {
			advancedPanel.setAttribute('hidden', 'true');
		} else {
			advancedPanel.removeAttribute('hidden');
		}
	});

	submitBtn.addEventListener('click', () => {
		const path = selectedPath;
		const desc = descInput.value.trim();
		if (!path) {
			errorMsg.style.display = '';
			setText(errorMsg, 'Select a project or add one.');
			return;
		}
		if (!desc) {
			errorMsg.style.display = '';
			setText(errorMsg, 'Description is required.');
			descInput.focus();
			return;
		}
		errorMsg.style.display = 'none';
		sendMessage({ type: 'CreateTask', project_path: path, description: desc });
		showView('dashboard');
	});

	const cancelBtn = el('button', {
		class: 'btn-cancel',
		type: 'button',
		onclick: () => {
			detachPopoverListeners();
			showView('dashboard');
		},
	}, 'Cancel');

	const form = el('div', { class: 'new-task-form' },
		el('div', { class: 'page-title' }, 'New Task'),
		el('div', { class: 'page-subtitle' }, 'Launch a Claude Code agent in an isolated worktree'),
		el('div', { class: 'form-group' },
			el('label', { class: 'form-label' }, 'Project'),
			dropdownMount,
			el('div', { class: 'form-hint' }, 'Select a saved project or add a new one'),
		),
		el('div', { class: 'form-group' },
			el('label', { class: 'form-label', for: 'input-task-description' }, 'Task Description'),
			descInput,
			el('div', { class: 'form-hint' }, 'Be specific. The agent will create an isolated worktree and execute this task.'),
		),
		el('div', { class: 'form-group' },
			advancedToggle,
			advancedPanel,
		),
		errorMsg,
		el('div', { class: 'form-actions' },
			cancelBtn,
			submitBtn,
		),
	);

	container.appendChild(form);
}

// ---------------------------------------------------------------------------
// View router
// ---------------------------------------------------------------------------

function showView(name, taskId) {
	if (cleanupProjectDropdown) {
		cleanupProjectDropdown();
		cleanupProjectDropdown = null;
	}

	currentView = name;
	if (taskId !== undefined) selectedTaskId = taskId;

	// Notify Rust of view switch
	sendMessage({ type: 'SwitchView', view: name });

	// Hide all content pages
	const pages = document.querySelectorAll('.content-page');
	for (const page of pages) {
		page.classList.remove('active');
	}

	// Show target page
	const target = document.getElementById('view-' + name);
	if (target) target.classList.add('active');

	// Update sidebar nav active states
	const navItems = document.querySelectorAll('.nav-item');
	for (const item of navItems) {
		const itemView = item.dataset.view;
		if (itemView === name || (name === 'task-detail' && itemView === 'dashboard')) {
			item.classList.add('active');
		} else {
			item.classList.remove('active');
		}
	}

	renderCurrentView();
}

function renderCurrentView() {
	renderSidebar();
	switch (currentView) {
		case 'dashboard':
			renderDashboard();
			break;
		case 'task-detail':
			renderTaskDetail();
			break;
		case 'new-task':
			renderNewTask();
			break;
		default:
			break;
	}
}

// ---------------------------------------------------------------------------
// Prompt input setup
// ---------------------------------------------------------------------------

function setupPromptInput() {
	const input = document.getElementById('prompt-input');
	if (!input) return;
	input.addEventListener('keydown', (e) => {
		if (e.key === 'Enter' && !e.shiftKey) {
			e.preventDefault();
			const text = input.value.trim();
			if (text && selectedTaskId) {
				sendMessage({ type: 'SendInput', task_id: selectedTaskId, data: text + '\n' });
				input.value = '';
			}
		}
	});
}

// ---------------------------------------------------------------------------
// IPC: Rust → JS entry points
// ---------------------------------------------------------------------------

function loadDriverState(state) {
	debug('loadDriverState', state);
	driverState = {
		tasks: state.tasks || [],
		stats: state.stats || { running: 0, completed: 0, failed: 0, total_tokens: 0 },
		projects: state.projects || [],
	};
	renderCurrentView();
}

function receiveDriverMessage(msg) {
	debug('receiveDriverMessage', msg);
	const type = msg.type;
	if (type === 'project_added' || type === 'project_removed' || type === 'projects') {
		driverState.projects = msg.projects || [];
		if (currentView === 'new-task') {
			renderNewTask();
		}
	} else if (type === 'task_created') {
		sendMessage({ type: 'RequestState' });
	} else if (type === 'state') {
		loadDriverState(msg);
	} else if (type === 'error') {
		debug('IPC error:', msg.message);
	}
}

function appendTaskOutput(taskId, data) {
	debug('appendTaskOutput', taskId, data ? data.length : 0);
	if (!taskOutputBuffers[taskId]) {
		taskOutputBuffers[taskId] = '';
	}
	taskOutputBuffers[taskId] += data;

	// Only update DOM if this task is currently displayed
	if (currentView === 'task-detail' && selectedTaskId === taskId) {
		const outputContent = document.getElementById('output-content');
		const outputArea = document.getElementById('output-area');
		if (outputContent) {
			// Safe: renderOutputHtml escapes all user content before wrapping spans
			outputContent.innerHTML = renderOutputHtml(taskOutputBuffers[taskId]);
			if (outputArea && !userScrolledUp) {
				outputArea.scrollTop = outputArea.scrollHeight;
			}
		}
	}
}

function updateTaskState(taskId, state, aiState) {
	debug('updateTaskState', taskId, state, aiState);
	const task = driverState.tasks.find(t => t.id === taskId);
	if (task) {
		task.state = state;
		if (aiState !== undefined) task.ai_state = aiState;
	}
	renderCurrentView();
}

function taskCreated(task) {
	debug('taskCreated', task);
	// Remove any existing entry with same id (idempotent)
	driverState.tasks = driverState.tasks.filter(t => t.id !== task.id);
	driverState.tasks.unshift(task);
	renderCurrentView();
}

function taskExited(taskId, exitCode) {
	debug('taskExited', taskId, exitCode);
	const task = driverState.tasks.find(t => t.id === taskId);
	if (task) {
		task.state = exitCode === 0 ? 'Completed' : 'Failed';
		task.exit_code = exitCode;
		task.completed_at = Math.floor(Date.now() / 1000);
	}
	renderCurrentView();
}

// ---------------------------------------------------------------------------
// Elapsed time ticker
// ---------------------------------------------------------------------------

function tickElapsed() {
	const elapsedEls = document.querySelectorAll('[data-created-at]');
	for (const node of elapsedEls) {
		const createdAt = parseInt(node.dataset.createdAt, 10);
		if (!isNaN(createdAt)) {
			setText(node, formatElapsed(createdAt));
		}
	}
}

// ---------------------------------------------------------------------------
// Bootstrap
// ---------------------------------------------------------------------------

document.addEventListener('DOMContentLoaded', () => {
	// Wire up static HTML sidebar controls
	const btnNewTask = document.getElementById('btn-new-task');
	if (btnNewTask) {
		btnNewTask.addEventListener('click', () => showView('new-task'));
	}
	const navDashboard = document.getElementById('nav-dashboard');
	if (navDashboard) {
		navDashboard.addEventListener('click', () => showView('dashboard'));
	}

	renderCurrentView();

	// Request initial state from Rust
	sendMessage({ type: 'RequestState' });

	// Tick elapsed timers every second
	setInterval(tickElapsed, 1000);
});
