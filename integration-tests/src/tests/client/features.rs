//! Collection of feature specific tests

mod access_key_nonce_for_implicit_accounts;
mod account_id_in_function_call_permission;
mod adversarial_behaviors;
mod cap_max_gas_price;
mod chunk_nodes_cache;
mod chunk_validation;
#[cfg(feature = "protocol_feature_fix_contract_loading_cost")]
mod fix_contract_loading_cost;
mod fix_storage_usage;
mod flat_storage;
mod in_memory_tries;
mod increase_deployment_cost;
mod increase_storage_compute_cost;
mod limit_contract_functions_number;
mod lower_storage_key_limit;
mod uncvm;
mod restore_receipts_after_fix_apply_chunks;
mod restrict_tla;
mod wallet_contract;
mod zero_balance_account;
