# Sourced (POSIX sh) by every Lefthook-generated Git hook through the `rc:` key
# of lefthook.yml, from the worktree root, before the hook calls "$LEFTHOOK_BIN".
# It routes that call through scripts/ci/lefthook_serial.py, which runs the same
# lefthook the hook would have run but makes partially staged pre-commits in
# different worktrees take turns (see that file for why). A caller's own
# LEFTHOOK_BIN is kept in AWAKEN_LEFTHOOK_BIN and restored for lefthook.
if [ -f scripts/ci/lefthook_serial.py ] && command -v python3 >/dev/null 2>&1; then
  case "${LEFTHOOK_BIN-}" in
    */lefthook_serial.py) ;;
    *) AWAKEN_LEFTHOOK_BIN="${LEFTHOOK_BIN-}"; export AWAKEN_LEFTHOOK_BIN ;;
  esac
  LEFTHOOK_BIN="$PWD/scripts/ci/lefthook_serial.py"
fi
