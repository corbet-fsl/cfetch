#!/usr/bin/env bash
# Provider-independent checks; run on a build worker with existing tools.
set -euo pipefail
exec 2>&1
cd "$(dirname "${BASH_SOURCE[0]}")/.."
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-1}"
export RUST_TEST_THREADS="${RUST_TEST_THREADS:-1}"
python_bin="${CFETCH_POLICY_PYTHON:-python3}"

# Test binaries that link the C++ runtime (onnxruntime in embed_model_cli) must find libstdc++ where the host has no FHS library path (a Nix host): the compiler
# says where it is. Only the `cargo test` runs get it, never the whole job.
cxx_runtime() {
  local lib
  lib=$(cc -print-file-name=libstdc++.so.6 2>/dev/null || true)
  if [[ $lib == /* && -e $lib ]]; then LD_LIBRARY_PATH="$(dirname "$lib")${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" "$@"; else "$@"; fi
}

if (($# == 0)); then
  set -- catalog rust
fi
for check in "$@"; do
  case "$check" in
    ci-config)
      "$python_bin" .ci/test_hosted.py
      "$python_bin" .ci/check-ci.py
      ;;
    release-guards)
      "$python_bin" -m unittest -v scripts.test_release
      ;;
    catalog)
      bash scripts/check-packaging-variants.sh
      echo 'catalog: variant-matrix validation'
      bash scripts/variant-matrix.sh >/dev/null
      echo 'catalog: python unit gates'
      "$python_bin" -m unittest -v scripts.test_stage_local_inference scripts.test_variant_matrix scripts.test_ci_variants scripts.test_release.ReleaseTests.test_native_host_spellings
      ;;
    staging)
      rustc --version
      cxx_runtime cargo test --locked --bin cfetch staging
      cxx_runtime cargo test --locked --bin cfetch init::tests
      cxx_runtime cargo test --locked --bin cfetch import::tests
      cxx_runtime cargo test --locked --bin cfetch index::tests
      ;;
    memory)
      rustc --version
      cxx_runtime cargo test --locked --features embedded-embeddings --bin cfetch
      ;;
    rust)
      version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)
      case "$version" in
        0.*) ;;
        *) echo "cfetch 1.0+ is operator-blocked (Cargo.toml has $version)" >&2; exit 1 ;;
      esac
      rustc --version
      cxx_runtime cargo test --locked
      cargo check --all-targets --all-features --locked
      cxx_runtime cargo test --locked --features embedded-embeddings --test embed_model_cli
      cxx_runtime cargo test --locked --features embedded-embeddings embedded_embed::tests
      cargo clippy --all-targets --all-features --locked -- -D warnings
      ;;
    variants)
      # Native Linux compilation only. This does not qualify any accelerator.
      [[ $(uname -s) == Linux ]] || { echo 'variants requires Linux' >&2; exit 2; }
      architecture=$(uname -m)
      matrix=$(bash scripts/variant-matrix.sh)
      selected=$(jq -c --arg arch "$architecture" '.include[] | select(.os == "linux" and .arch == $arch)' <<<"$matrix")
      [[ -n $selected ]] || { echo 'No native Linux variant for this worker' >&2; exit 2; }
      while IFS= read -r row; do
        variant=$(jq -r '.id' <<<"$row")
        target=$(jq -r '.target // ""' <<<"$row")
        features=$(jq -r '.cargo_features // ""' <<<"$row")
        # Match ci.yml's release-check arguments, including an empty feature list.
        cargo_args=(check --release --locked)
        if [[ -n $target ]]; then
          cargo_args+=(--target "$target")
        fi
        cargo_args+=(--features "$features")
        CFETCH_VARIANT="$variant" cargo "${cargo_args[@]}"
      done <<<"$selected"
      ;;
    licenses)
      cargo deny check advisories licenses
      generated=$(mktemp)
      trap 'rm -f -- "$generated"' EXIT
      bash scripts/generate-third-party-licenses.sh "$generated"
      diff -u THIRD-PARTY-LICENSES.txt "$generated"
      ;;
    ort-foundation-build)
      "$python_bin" experiments/npu-ort-foundation/build_cached_probe.py
      ;;
    ort-foundation-cpu)
      "$python_bin" -m unittest discover -s experiments/npu-ort-foundation -p test_pinned_cache.py -v
      if [[ -n "${CFETCH_FOUNDATION_CPU_INPUT:-}" ]]; then
        "$python_bin" -m unittest discover -s experiments/npu-ort-foundation -p test_cpu_input.py -v
      fi
      "$python_bin" experiments/npu-ort-foundation/run_cached_cpu.py
      ;;
    hook-deadline)
      cxx_runtime cargo test --locked --bin cfetch runtime_status::tests
      cxx_runtime cargo test --locked --bin cfetch hooks::tests
      cxx_runtime cargo test --locked --bin cfetch daemon::tests
      cxx_runtime cargo test --locked --test hook_deadline
      cxx_runtime cargo test --locked --test freshness_cli
      cargo clippy --locked --bin cfetch -- -D warnings
      ;;
    freshness)
      cxx_runtime cargo test --locked --bin cfetch serve::tests
      cxx_runtime cargo test --locked --bin cfetch daemon::tests
      cxx_runtime cargo test --locked --bin cfetch mcp::tests
      cxx_runtime cargo test --locked --test freshness_cli
      cargo check --locked --features embedded-embeddings --test local_memory
      cxx_runtime cargo test --locked --test coherence
      cxx_runtime cargo test --locked --test mcp_stdio
      cargo clippy --locked --bin cfetch -- -D warnings
      ;;
    native-worker)
      rustc --version
      cxx_runtime cargo test --locked --features native-openvino --bin cfetch inference_governor::tests
      cxx_runtime cargo test --locked --features native-openvino --bin cfetch native_worker::tests
      cxx_runtime cargo test --locked --features native-openvino --bin cfetch native_adapter::tests
      cxx_runtime cargo test --locked --features native-openvino --bin cfetch native_serving::tests
      cxx_runtime cargo test --locked --features native-openvino --bin cfetch native_manifest::tests
      cxx_runtime cargo test --locked --features native-openvino --bin cfetch native_http::tests
      cxx_runtime cargo test --locked --features native-openvino --bin cfetch local_adapter::tests
      cxx_runtime cargo test --locked --features native-openvino --bin cfetch embed::tests
      cxx_runtime cargo test --locked --features native-openvino --bin cfetch runtime_status::tests
      cxx_runtime cargo test --locked --features native-openvino --bin cfetch rerank::tests
      cxx_runtime cargo test --locked --features native-openvino --test native_worker_cli
      cargo clippy --locked --features native-openvino --bin cfetch -- -D warnings
      ;;
    native-governor)
      rustc --version
      cxx_runtime cargo test --locked --bin cfetch inference_governor::tests
      cxx_runtime cargo test --locked --bin cfetch local_adapter::tests
      cargo clippy --locked --bin cfetch -- -D warnings
      ;;
    admission-origin)
      export PYTHONDONTWRITEBYTECODE=1
      cxx_runtime cargo test --locked --bin cfetch local_inference::tests
      cxx_runtime cargo test --locked --bin cfetch embedding_profile::tests
      cxx_runtime "$python_bin" -m unittest -v scripts.test_stage_local_inference scripts.test_apply_admission_activation
      cxx_runtime "$python_bin" -m unittest -v packages.openvino.tests.test_manifest
      (
        cd experiments/embedding-profile
        cxx_runtime "$python_bin" -m unittest -v test_admission_origin
        cxx_runtime "$python_bin" cross_backend_eval.py --verify-implementation-bundle
        cxx_runtime "$python_bin" cross_backend_eval.py --verify-release-registry
      )
      ;;
    governor)
      # Native deadline and persisted load policy; no model/runtime dependency.
      "$python_bin" -m unittest -v \
        packages.openvino.tests.test_inference_governor \
        packages.openvino.tests.test_native_deadline
      ;;
    profile)
      # The caller supplies the environment from requirements-lock.txt.
      # No dependency installation or physical-device probe occurs here.
      (
        cd experiments/embedding-profile
        cxx_runtime "$python_bin" -m unittest -v \
          test_cross_backend_eval.py test_export_adapter_cache.py \
          test_kat_host_runner.py test_measurement_bundle.py \
          test_physical_evidence.py test_physical_checkpoint.py \
          test_openvino_scope_keys.py test_scifact_contract.py \
          test_final_package_conformance.py test_admission_transaction.py
        HF_HUB_DISABLE_TELEMETRY=1 cxx_runtime "$python_bin" scifact_contract.py
        cxx_runtime "$python_bin" cross_backend_eval.py --verify-implementation-bundle
        HF_HUB_DISABLE_TELEMETRY=1 cxx_runtime "$python_bin" cross_backend_eval.py --verify-release-registry
      )
      cxx_runtime "$python_bin" -m unittest discover -s packages/openvino/tests -v
      cxx_runtime "$python_bin" -m unittest discover -s experiments/memory-retrieval -v
      cxx_runtime "$python_bin" -m unittest scripts/test_apply_admission_activation.py -v
      for command in \
        experiments/embedding-profile/export_adapter_cache.py \
        experiments/embedding-profile/cross_backend_eval.py \
        experiments/embedding-profile/final_package_conformance.py \
        experiments/embedding-profile/admission_transaction.py \
        experiments/embedding-profile/physical_evidence.py \
        scripts/apply_admission_activation.py; do
        cxx_runtime "$python_bin" "$command" --help >/dev/null
      done
      ;;
    *) echo "Unknown check: $check (ci-config release-guards catalog rust variants licenses governor profile ort-foundation-build ort-foundation-cpu)" >&2; exit 2 ;;
  esac
done
