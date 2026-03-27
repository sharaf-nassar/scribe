# Scribe shell integration — zsh bootstrap
# This file is sourced as $ZDOTDIR/.zshenv because Scribe temporarily
# redirected ZDOTDIR. Restore it immediately so zsh finds the user's
# real startup files for the rest of the startup sequence.

# Restore original ZDOTDIR
if [[ -n "${SCRIBE_ORIG_ZDOTDIR+x}" ]]; then
	ZDOTDIR="$SCRIBE_ORIG_ZDOTDIR"
	[[ -z "$ZDOTDIR" ]] && unset ZDOTDIR
	unset SCRIBE_ORIG_ZDOTDIR
else
	unset ZDOTDIR
fi

# Source user's real .zshenv
if [[ -f "${ZDOTDIR:-$HOME}/.zshenv" ]]; then
	source "${ZDOTDIR:-$HOME}/.zshenv"
fi

# Source the integration script (uses SCRIBE_SHELL_INTEGRATION_DIR set by server)
# The script path is relative to this .zshenv file
_scribe_self="${(%):-%x}"
_scribe_dir="${_scribe_self:A:h}"
if [[ -f "${_scribe_dir}/scribe.zsh" ]]; then
	source "${_scribe_dir}/scribe.zsh"
fi
unset _scribe_self _scribe_dir
