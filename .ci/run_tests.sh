#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

CASPER_CLI_VERSION="${CASPER_CLI_VERSION:-0.3.0}"
CASPER_DEVNET_VERSION="${CASPER_DEVNET_VERSION:-0.6.0}"

CASPER_CLI_BASE_URL="${CASPER_CLI_BASE_URL:-https://github.com/veles-labs/casper-cli/releases/download/${CASPER_CLI_VERSION}}"
CASPER_DEVNET_BASE_URL="${CASPER_DEVNET_BASE_URL:-https://github.com/veles-labs/casper-devnet/releases/download/v${CASPER_DEVNET_VERSION}}"

WORK_DIR="${CSPR_WORK_DIR:-${RUNNER_TEMP:-/tmp}/casper-ci}"
BIN_DIR="${CSPR_BIN_DIR:-$WORK_DIR/bin}"
DOWNLOAD_DIR="$WORK_DIR/downloads"
EXTRACT_DIR="$WORK_DIR/extract"
CSPR_STORAGE_ROOT="${CSPR_STORAGE_ROOT:-$WORK_DIR/casper-cli}"

mkdir -p "$BIN_DIR" "$DOWNLOAD_DIR" "$EXTRACT_DIR" "$CSPR_STORAGE_ROOT"
export PATH="$BIN_DIR:$PATH"

log() {
  printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"
}

wait_for_exit() {
  local pid="$1"
  local timeout="${2:-20}"
  local waited=0
  while kill -0 "$pid" >/dev/null 2>&1; do
    if [ "$waited" -ge "$timeout" ]; then
      return 1
    fi
    sleep 1
    waited=$((waited + 1))
  done
  return 0
}

detect_target_triple() {
  local os arch
  os="$(uname -s | tr '[:upper:]' '[:lower:]')"
  arch="$(uname -m)"

  case "$arch" in
    x86_64 | amd64)
      arch="x86_64"
      ;;
    aarch64 | arm64)
      arch="aarch64"
      ;;
    *)
      log "Unsupported architecture: $arch"
      exit 1
      ;;
  esac

  case "$os" in
    linux)
      os="unknown-linux-gnu"
      ;;
    darwin)
      os="apple-darwin"
      ;;
    *)
      log "Unsupported OS: $os"
      exit 1
      ;;
  esac

  echo "${arch}-${os}"
}

install_tarball_bin() {
  local name="$1"
  local tarball="$2"
  local url="$3"
  local bin_name="$4"

  local dest="$DOWNLOAD_DIR/$tarball"
  if [ ! -f "$dest" ]; then
    log "Downloading $tarball"
    curl -fsSL "$url" -o "$dest"
  fi

  if [ ! -x "$BIN_DIR/$bin_name" ]; then
    local tmp_dir="$EXTRACT_DIR/$name"
    rm -rf "$tmp_dir"
    mkdir -p "$tmp_dir"
    tar -xzf "$dest" -C "$tmp_dir"
    local bin_path="$tmp_dir/$bin_name"
    if [ ! -f "$bin_path" ]; then
      bin_path="$(find "$tmp_dir" -type f -name "$bin_name" -print -quit)"
    fi
    if [ -z "$bin_path" ]; then
      log "Failed to locate $bin_name in $tarball"
      exit 1
    fi
    install -m 0755 "$bin_path" "$BIN_DIR/$bin_name"
  fi
}

TARGET_TRIPLE="$(detect_target_triple)"
CLI_TARBALL="casper-cli-${CASPER_CLI_VERSION}-${TARGET_TRIPLE}.tar.gz"
DEVNET_TARBALL="casper-devnet-${CASPER_DEVNET_VERSION}-${TARGET_TRIPLE}.tar.gz"

NEED_CLI=1
NEED_DEVNET=1
if command -v casper-cli >/dev/null 2>&1; then
  NEED_CLI=0
  log "Found casper-cli in PATH: $(command -v casper-cli)"
fi
if command -v casper-devnet >/dev/null 2>&1; then
  NEED_DEVNET=0
  log "Found casper-devnet in PATH: $(command -v casper-devnet)"
fi

if [ "$NEED_CLI" -eq 1 ] || [ "$NEED_DEVNET" -eq 1 ]; then
  if ! command -v curl >/dev/null 2>&1; then
    log "curl is required to download casper tools"
    exit 1
  fi
fi

if [ "$NEED_CLI" -eq 1 ]; then
  install_tarball_bin \
    "casper-cli" \
    "$CLI_TARBALL" \
    "${CASPER_CLI_BASE_URL}/${CLI_TARBALL}" \
    "casper-cli"
else
  log "Skipping casper-cli download"
fi

if [ "$NEED_DEVNET" -eq 1 ]; then
  install_tarball_bin \
    "casper-devnet" \
    "$DEVNET_TARBALL" \
    "${CASPER_DEVNET_BASE_URL}/${DEVNET_TARBALL}" \
    "casper-devnet"
else
  log "Skipping casper-devnet download"
fi

log "casper-cli version (host)"
casper-cli --version

DEVNET_LOG="${DEVNET_LOG:-$WORK_DIR/devnet.log}"
DEVNET_ERR_LOG="${DEVNET_ERR_LOG:-$WORK_DIR/devnet.err.log}"
DEVNET_NETWORK_NAME="${DEVNET_NETWORK_NAME:-casper-devnet-$$}"
DEVNET_START_CMD="${DEVNET_START_CMD:-casper-devnet start --network-name ${DEVNET_NETWORK_NAME}}"
DEVNET_READY_CMD="${DEVNET_READY_CMD:-casper-devnet is-ready --network-name ${DEVNET_NETWORK_NAME}}"
DEVNET_KILL_TIMEOUT="${DEVNET_KILL_TIMEOUT:-20}"

log "Checking devnet assets"
if casper-devnet assets list >/dev/null 2>&1; then
  log "Devnet assets already present, skipping pull"
else
  log "No devnet assets found, pulling"
  casper-devnet assets pull
  casper-devnet assets list
fi

log "Starting devnet: ${DEVNET_START_CMD}"
if command -v setsid >/dev/null 2>&1; then
  setsid bash -c "$DEVNET_START_CMD" >"$DEVNET_LOG" 2>"$DEVNET_ERR_LOG" &
  DEVNET_PID=$!
  DEVNET_PGID=$DEVNET_PID
else
  bash -c "$DEVNET_START_CMD" >"$DEVNET_LOG" 2>"$DEVNET_ERR_LOG" &
  DEVNET_PID=$!
  DEVNET_PGID=""
fi

cleanup_devnet() {
  if [ "${DEVNET_KEEP_PROCESS:-}" = "1" ]; then
    return
  fi
  if ! kill -0 "$DEVNET_PID" >/dev/null 2>&1; then
    return
  fi
  log "Stopping devnet (SIGINT)"
  if [ -n "${DEVNET_PGID:-}" ]; then
    kill -INT "-$DEVNET_PGID" || true
  else
    kill -INT "$DEVNET_PID" || true
    if command -v pkill >/dev/null 2>&1; then
      pkill -INT -P "$DEVNET_PID" || true
    fi
  fi
  if ! wait_for_exit "$DEVNET_PID" "$DEVNET_KILL_TIMEOUT"; then
    log "Devnet still running, sending SIGTERM"
    if [ -n "${DEVNET_PGID:-}" ]; then
      kill -TERM "-$DEVNET_PGID" || true
    else
      kill -TERM "$DEVNET_PID" || true
      if command -v pkill >/dev/null 2>&1; then
        pkill -TERM -P "$DEVNET_PID" || true
      fi
    fi
    wait_for_exit "$DEVNET_PID" "$DEVNET_KILL_TIMEOUT" || true
  fi
}
trap cleanup_devnet EXIT

DEVNET_WALLET_NAME="${DEVNET_WALLET_NAME:-devnet}"
DEVNET_ACCOUNT_NAME="${DEVNET_ACCOUNT_NAME:-user-1}"
DEVNET_ACCOUNT_TEMPLATE="${DEVNET_ACCOUNT_TEMPLATE:-user-{index}}"
DEVNET_ACCOUNT_START="${DEVNET_ACCOUNT_START:-1}"
DEVNET_ACCOUNT_COUNT="${DEVNET_ACCOUNT_COUNT:-1}"
DEVNET_SEED="${DEVNET_SEED:-default}"
DEVNET_DOMAIN="${DEVNET_DOMAIN:-devnet}"

wallet_path="$CSPR_STORAGE_ROOT/wallets/${DEVNET_WALLET_NAME}.json"
echo $wallet_path
if [ ! -f "$wallet_path" ]; then
  log "Creating devnet wallet $DEVNET_WALLET_NAME"
  casper-cli wallet create "$DEVNET_WALLET_NAME" \
    --seed "$DEVNET_SEED" \
    --domain "$DEVNET_DOMAIN" \
    --no-interactive \
    --unencrypted \
    --file-storage "$CSPR_STORAGE_ROOT"
  log "Deriving devnet accounts"
  casper-cli wallet derive "$DEVNET_WALLET_NAME" \
    --name "$DEVNET_ACCOUNT_TEMPLATE" \
    --start "$DEVNET_ACCOUNT_START" \
    --count "$DEVNET_ACCOUNT_COUNT" \
    --no-interactive \
    --file-storage "$CSPR_STORAGE_ROOT"
fi


log "Waiting for devnet readiness via casper-devnet"
READY_ATTEMPTS="${DEVNET_READY_ATTEMPTS:-60}"
READY_SLEEP="${DEVNET_READY_SLEEP:-2}"
for ((i = 1; i <= READY_ATTEMPTS; i++)); do
  if $DEVNET_READY_CMD >/dev/null 2>&1; then
    log "Devnet is ready"
    break
  fi
  if [ "$i" -eq "$READY_ATTEMPTS" ]; then
    log "Devnet did not become ready in time"
    if [ -f "$DEVNET_LOG" ]; then
      log "Devnet logs:"
      cat "$DEVNET_LOG"
    fi
    if [ -f "$DEVNET_ERR_LOG" ]; then
      log "Devnet stderr logs:"
      cat "$DEVNET_ERR_LOG"
    fi
    exit 1
  fi
  sleep "$READY_SLEEP"
done

log "Running tests"
cd "$ROOT_DIR"
cargo test --all --locked

log "Running database migrations"
(cd "$ROOT_DIR" && cargo run -- migrate)
