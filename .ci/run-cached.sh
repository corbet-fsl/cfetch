#!/usr/bin/env bash
# Execute the manifest's selected checks with content-keyed result reuse.
set -euo pipefail
# Merge stderr into the captured stdout stream (Crow preserves stdout only).
# Output routing only; commands and gates unchanged.
exec 2>&1
: "${CCID_BIN:?Missing verified ccid job runtime}"
: "${CI_COMMIT_SHA:?Missing verified source revision}"
: "${CI_REPOSITORY_URL:?Missing canonical repository identity}"
: "${CHECKS:?Missing declared check selection}"
if [ "$CHECKS" != "ci-config,catalog,rust,memory,variants,licenses,profile,governor,release-guards,staging,admission-origin,freshness,hook-deadline" ]; then
  echo "CHECKS must be exactly 'ci-config,catalog,rust,memory,variants,licenses,profile,governor,release-guards,staging,admission-origin,freshness,hook-deadline', got: $CHECKS" >&2
  exit 2
fi
cache_root=${CI_CACHE_ROOT:-${CARGO_HOME:-}}
[[ $cache_root = /* ]] || { echo 'An absolute persistent CI_CACHE_ROOT or CARGO_HOME is required' >&2; exit 2; }
namespace=$(printf '%s' "$CI_REPOSITORY_URL" | sha256sum | cut -c1-24)
state="$cache_root/ccid-cached/$namespace"
toolchains=${CI_TOOLCHAIN_ROOT:-$cache_root/ccid-cached/toolchains}
[[ $toolchains = /* ]] || { echo 'CI_TOOLCHAIN_ROOT must be absolute' >&2; exit 2; }
mkdir -p "$state" "$toolchains"
exec 9>"$state/lock"
flock -w "${CI_TIMEOUT:-2700}" 9
mkdir -p "$state/moon-cache/hashes" "$state/moon-cache/outputs" "$state/moon-home"
mkdir -p .moon/cache
# Tool installation is an explicit part of this job, in persistent CI storage.
# Pinned nixpkgs revision doubles as the version lock: jq 1.8.2, shellcheck 0.11.0,
# actionlint 1.7.12, cargo-deny 0.20.2 (exact upstream pin), python312 3.12.13
# matching CI's python-version 3.12 for the hashed policy lock. The nixpkgs
# cargo-about (0.9.0) is NOT used; exact 0.9.2 is provisioned below.
tool_source=github:NixOS/nixpkgs/b7c2ada94fe99c15b0dbcf4d11fd7850b957a436
tool_paths=$(nix build --no-link --print-out-paths "$tool_source#moon" "$tool_source#proto" "$tool_source#nodejs" "$tool_source#python3" "$tool_source#python312" "$tool_source#cargo-deny" "$tool_source#cargo-about" "$tool_source#jq" "$tool_source#shellcheck" "$tool_source#actionlint")
for path in $tool_paths; do PATH="$path/bin:$PATH"; done
# Policy Python environment: the exact hashed lock from the checkout, installed
# once per lock revision into persistent CI storage (never inside the checkout,
# so cached runs mutate no source). Transitive closure comes from the lock itself.
policy_lock="$PWD/experiments/embedding-profile/requirements-lock.txt"
policy_venv="$state/policy-venv"
policy_lock_sha=$(sha256sum "$policy_lock" | cut -d' ' -f1)
if [[ ! -f "$policy_venv.lock-sha" || $(cat "$policy_venv.lock-sha") != "$policy_lock_sha" ]]; then
  rm -rf "$policy_venv"
  policy_seed=$(nix build --no-link --print-out-paths "$tool_source#python312")
  "$policy_seed/bin/python3" -m venv "$policy_venv"
  "$policy_venv/bin/python" -m pip install --require-hashes -r "$policy_lock"
  printf '%s' "$policy_lock_sha" > "$policy_venv.lock-sha"
fi
export CFETCH_POLICY_PYTHON="$policy_venv/bin/python"
export PROTO_HOME="$toolchains/proto" RUSTUP_HOME="$toolchains/rustup"
shared_cargo_home=${CARGO_HOME:-}
export CARGO_HOME="$toolchains/cargo" MOON_HOME="$state/moon-home"
export PROTO_AUTO_CLEAN=false PROTO_TELEMETRY=false PROTO_YES=true
export RUSTUP_INIT_SKIP_PATH_CHECK=yes RUSTUP_TOOLCHAIN=stable
export PATH="$(dirname "$CCID_BIN"):$CARGO_HOME/bin:$PROTO_HOME/shims:$PROTO_HOME/bin:$PATH"
export CI_JOBS="${CI_JOBS:-2}" CARGO_BUILD_JOBS="${CI_JOBS:-2}"
export RUST_TEST_THREADS="${CI_TEST_THREADS:-2}"
export CARGO_NET_GIT_FETCH_WITH_CLI=true
# Append to any git config the verified runtime already supplies through the environment.
git_config_base=${GIT_CONFIG_COUNT:-0}
export GIT_CONFIG_COUNT=$((git_config_base + 2))
export "GIT_CONFIG_KEY_$((git_config_base + 0))=url.https://git.corbet.ch/corbet-libs/.insteadOf"
export "GIT_CONFIG_VALUE_$((git_config_base + 0))=https://github.com/corbet-libs/"
export "GIT_CONFIG_KEY_$((git_config_base + 1))=url.https://git.corbet.ch/corbet-foss/.insteadOf"
export "GIT_CONFIG_VALUE_$((git_config_base + 1))=https://github.com/corbet-foss/"
# Shared-first toolchain lock: jobs share a provisioned toolchain and take
# exclusive only to provision missing pieces. Same lock file, so old
# whole-job-exclusive runners interoperate (they just serialize).
TOOLCHAIN_LOCK_FILE="$toolchains/install.lock"
TOOLCHAIN_LOCK_TIMEOUT="${CI_TIMEOUT:-2700}"
TOOLCHAIN_WANT_NIGHTLY=0
TOOLCHAIN_WASM_TARGETS=
# shellcheck source=toolchain-lock.sh
source "$(dirname "${BASH_SOURCE[0]}")/toolchain-lock.sh"
toolchain_ensure
cargo_bin=$("$CARGO_HOME/bin/rustup" which --toolchain stable cargo)
test -x "$cargo_bin"
export PATH="$(dirname "$cargo_bin"):$PATH"
# Exact license tools, under the per-repo lock (fd 9, already held). cargo-deny
# arrives via the pinned nixpkgs revision; assert it matches the CI
# install-action pin (0.20.2). Per-repo exact-tools root: no global mutation,
# same 0.9.2 pin and post-assert as before.
exact_tools="$state/exact-tools"
# Built from locked crates.io source (no verified third-party binary checksum
# in-repo); idempotent via version guard. Nothing touches the local checkout.
mkdir -p "$exact_tools/bin"
if [[ $("$exact_tools/bin/cargo-about" --version 2>/dev/null || true) != "cargo-about 0.9.2" ]]; then
  cargo install --locked --version 0.9.2 --features cli --root "$exact_tools" cargo-about
fi
[[ $("$exact_tools/bin/cargo-about" --version) == "cargo-about 0.9.2" ]] || { echo 'cargo-about 0.9.2 provisioning failed' >&2; exit 2; }
[[ $(cargo-deny --version) == *"cargo-deny 0.20.2"* ]] || { echo 'cargo-deny must be 0.20.2 (CI install-action pin)' >&2; exit 2; }
# Provisioned tools first: cache_tools entries and the checks below must resolve
# the exact versions (venv python 3.12, cargo-about 0.9.2), never nix fallbacks.
export PATH="$exact_tools/bin:$(dirname "$CFETCH_POLICY_PYTHON"):$PATH"
# Content fingerprints consumed by manifest cache_env: printed versions alone do
# not affect cache keys, these exports do.
export CFETCH_POLICY_FINGERPRINT="python-$("$CFETCH_POLICY_PYTHON" --version 2>&1 | cut -d' ' -f2)-freeze-$("$CFETCH_POLICY_PYTHON" -m pip freeze --exclude-editable | LC_ALL=C sort | sha256sum | cut -c1-16)"
export CFETCH_TOOL_FINGERPRINT="rustc-$(rustc -vV | sha256sum | cut -c1-16)-cargo-$(cargo --version)-jq-$(jq --version)-shellcheck-$(shellcheck --version | head -1)-actionlint-$(actionlint -version)-deny-$(cargo-deny --version)-about-$(cargo-about --version)"
export CCID_SOURCE_REVISION="$CI_COMMIT_SHA"
"$CCID_BIN" render --repo "$PWD" --check
proto --version
moon --version
rustc -vV
cargo --version
node --version
python3 --version
"$CFETCH_POLICY_PYTHON" --version
printf 'CFETCH_POLICY_FINGERPRINT=%s\n' "$CFETCH_POLICY_FINGERPRINT"
printf 'CFETCH_TOOL_FINGERPRINT=%s\n' "$CFETCH_TOOL_FINGERPRINT"
cargo-deny --version
cargo-about --version
jq --version
shellcheck --version
actionlint -version
"$CCID_BIN" cached --repo "$PWD" --check ci-config
"$CCID_BIN" cached --repo "$PWD" --check catalog
"$CCID_BIN" cached --repo "$PWD" --check rust
"$CCID_BIN" cached --repo "$PWD" --check memory
"$CCID_BIN" cached --repo "$PWD" --check variants
"$CCID_BIN" cached --repo "$PWD" --check profile
"$CCID_BIN" cached --repo "$PWD" --check governor
"$CCID_BIN" cached --repo "$PWD" --check release-guards
"$CCID_BIN" cached --repo "$PWD" --check staging
"$CCID_BIN" cached --repo "$PWD" --check admission-origin
"$CCID_BIN" cached --repo "$PWD" --check freshness
"$CCID_BIN" cached --repo "$PWD" --check hook-deadline
"$CCID_BIN" check --repo "$PWD" --check licenses
