#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

CASPER_CLI_VERSION="${CASPER_CLI_VERSION:-0.3.0}"
CASPER_DEVNET_VERSION="${CASPER_DEVNET_VERSION:-0.5.2}"

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

sha256_cmd() {
  if command -v sha256sum >/dev/null 2>&1; then
    echo "sha256sum"
    return 0
  fi
  if command -v shasum >/dev/null 2>&1; then
    echo "shasum -a 256"
    return 0
  fi
  return 1
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
  local sha="$4"
  local bin_name="$5"
  local sha_env="$6"

  local dest="$DOWNLOAD_DIR/$tarball"
  if [ ! -f "$dest" ]; then
    log "Downloading $tarball"
    curl -fsSL "$url" -o "$dest"
  fi

  if [ -n "$sha" ]; then
    local sha_cmd
    sha_cmd="$(sha256_cmd)" || {
      log "No sha256 tool found; cannot verify $tarball"
      exit 1
    }
    echo "$sha  $dest" | $sha_cmd -c -
  else
    log "Skipping checksum for $tarball (set ${sha_env} to verify)."
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

CLI_SHA=""
DEVNET_SHA=""
case "$TARGET_TRIPLE" in
  aarch64-apple-darwin)
    CLI_SHA="be60de774e14266db8454d238a1c1f3e499a39dc4002e499fe629b109a1939b3"
    DEVNET_SHA="b549c41245e4b5b4401c8b13d307be5a0cc22bae6539aa436883e1406b784fda"
    ;;
  aarch64-unknown-linux-gnu)
    CLI_SHA="3706e36f9a159632ae23e48fdc562e5f013bb5f400c0396ce183ada92f420076"
    DEVNET_SHA="2af025074826de28b0cecf0d2a2c0be61a9e339232ffdbdb260d51cbfd516b27"
    ;;
  x86_64-apple-darwin)
    CLI_SHA="0cd42b71abbfda90b23f7661911b5fad424a7453e3bae9da1cf37bd5ec141c7b"
    DEVNET_SHA="66df2266b6acc08a2c8d50989f49c6c43241d62048f60e5981526b6e2cbb0637"
    ;;
  x86_64-unknown-linux-gnu)
    CLI_SHA="${CASPER_CLI_SHA256:-}"
    DEVNET_SHA="f73950d20a0a377f2c940a5a15dfee1a8fe1e04d13af938efeabdcc8b67d4909"
    ;;
  *)
    log "Unsupported target triple: $TARGET_TRIPLE"
    exit 1
    ;;
esac

if [ -n "${CASPER_CLI_SHA256:-}" ]; then
  CLI_SHA="$CASPER_CLI_SHA256"
fi
if [ -n "${CASPER_DEVNET_SHA256:-}" ]; then
  DEVNET_SHA="$CASPER_DEVNET_SHA256"
fi

if [ "$NEED_CLI" -eq 1 ]; then
  install_tarball_bin \
    "casper-cli" \
    "$CLI_TARBALL" \
    "${CASPER_CLI_BASE_URL}/${CLI_TARBALL}" \
    "$CLI_SHA" \
    "casper-cli" \
    "CASPER_CLI_SHA256"
else
  log "Skipping casper-cli download"
fi

if [ "$NEED_DEVNET" -eq 1 ]; then
  install_tarball_bin \
    "casper-devnet" \
    "$DEVNET_TARBALL" \
    "${CASPER_DEVNET_BASE_URL}/${DEVNET_TARBALL}" \
    "$DEVNET_SHA" \
    "casper-devnet" \
    "CASPER_DEVNET_SHA256"
else
  log "Skipping casper-devnet download"
fi

log "casper-cli version (host)"
casper-cli --version



DEVNET_LOG="${DEVNET_LOG:-$WORK_DIR/devnet.log}"
DEVNET_START_CMD="${DEVNET_START_CMD:-casper-devnet start}"
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
  setsid bash -c "$DEVNET_START_CMD" >"$DEVNET_LOG" 2>&1 &
  DEVNET_PID=$!
  DEVNET_PGID=$DEVNET_PID
else
  bash -c "$DEVNET_START_CMD" >"$DEVNET_LOG" 2>&1 &
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
  log "Stopping devnet (SIGINT)"∂
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


ACCOUNT_REF="${DEVNET_WALLET_NAME}:${DEVNET_ACCOUNT_NAME}"
VIEW_CMD=(casper-cli account view "$ACCOUNT_REF" --no-interactive --file-storage "$CSPR_STORAGE_ROOT")

log "Waiting for devnet readiness via ${ACCOUNT_REF}"
READY_ATTEMPTS="${DEVNET_READY_ATTEMPTS:-60}"
READY_SLEEP="${DEVNET_READY_SLEEP:-2}"
for ((i = 1; i <= READY_ATTEMPTS; i++)); do
  if "${VIEW_CMD[@]}" >/dev/null 2>&1; then
    log "Devnet is ready"
    break
  fi
  if [ "$i" -eq "$READY_ATTEMPTS" ]; then
    log "Devnet did not become ready in time"
    if [ -f "$DEVNET_LOG" ]; then
      log "Devnet logs:"
      cat "$DEVNET_LOG"
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
