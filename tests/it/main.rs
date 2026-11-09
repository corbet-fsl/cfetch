//! Consolidated integration-test binary: one module per former `tests/*.rs` file.
//! Test names keep the former file name as their first segment
//! (`<module>::<test>`), so `--test it <module>` selects exactly the tests
//! that `--test <module>` used to select.

mod coherence;
mod doctor_cli;
mod embed_model_cli;
mod freshness_cli;
mod hook_deadline;
mod local_memory;
mod mcp_stdio;
mod memory_eval_cli;
mod native_worker_cli;
