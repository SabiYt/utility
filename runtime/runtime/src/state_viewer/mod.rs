use crate::unc_primitives::version::PROTOCOL_VERSION;
use crate::receipt_manager::ReceiptManager;
use crate::{actions::execute_function_call, ext::RuntimeExt};
use unc_crypto::{KeyType, PublicKey};
use unc_parameters::RuntimeConfigStore;
use unc_primitives::account::{AccessKey, Account};
use unc_primitives::borsh::BorshDeserialize;
use unc_primitives::hash::CryptoHash;
use unc_primitives::receipt::ActionReceipt;
use unc_primitives::runtime::apply_state::ApplyState;
use unc_primitives::runtime::migration_data::{MigrationData, MigrationFlags};
use unc_primitives::transaction::FunctionCallAction;
use unc_primitives::trie_key::trie_key_parsers;
use unc_primitives::types::{AccountId, EpochInfoProvider, Gas};
use unc_primitives::views::{ChipView, StateItem, ViewApplyState, ViewStateResult};
use unc_primitives_core::config::ViewConfig;
use unc_store::{get_access_key, get_account, get_code, TrieUpdate};
use unc_vm_runner::logic::ReturnData;
use unc_vm_runner::ContractCode;
use std::{str, sync::Arc, time::Instant};
use tracing::debug;
use crate::state_viewer::errors::ViewChipError;

pub mod errors;

pub struct TrieViewer {
    /// Upper bound of the byte size of contract state that is still viewable. None is no limit
    state_size_limit: Option<u64>,
    /// Gas limit used when when handling call_function queries.
    max_gas_burnt_view: Gas,
}

impl Default for TrieViewer {
    fn default() -> Self {
        let config_store = RuntimeConfigStore::new(None);
        let latest_runtime_config = config_store.get_config(PROTOCOL_VERSION);
        let max_gas_burnt = latest_runtime_config.wasm_config.limit_config.max_gas_burnt;
        Self { state_size_limit: None, max_gas_burnt_view: max_gas_burnt }
    }
}

impl TrieViewer {
    pub fn new(state_size_limit: Option<u64>, max_gas_burnt_view: Option<Gas>) -> Self {
        let max_gas_burnt_view =
            max_gas_burnt_view.unwrap_or_else(|| TrieViewer::default().max_gas_burnt_view);
        Self { state_size_limit, max_gas_burnt_view }
    }

    pub fn view_account(
        &self,
        state_update: &TrieUpdate,
        account_id: &AccountId,
    ) -> Result<Account, errors::ViewAccountError> {
        get_account(state_update, account_id)?.ok_or_else(|| {
            errors::ViewAccountError::AccountDoesNotExist {
                requested_account_id: account_id.clone(),
            }
        })
    }

    pub fn view_contract_code(
        &self,
        state_update: &TrieUpdate,
        account_id: &AccountId,
    ) -> Result<ContractCode, errors::ViewContractCodeError> {
        let account = self.view_account(state_update, account_id)?;
        get_code(state_update, account_id, Some(account.code_hash()))?.ok_or_else(|| {
            errors::ViewContractCodeError::NoContractCode {
                contract_account_id: account_id.clone(),
            }
        })
    }

    pub fn view_access_key(
        &self,
        state_update: &TrieUpdate,
        account_id: &AccountId,
        public_key: &PublicKey,
    ) -> Result<AccessKey, errors::ViewAccessKeyError> {
        get_access_key(state_update, account_id, public_key)?.ok_or_else(|| {
            errors::ViewAccessKeyError::AccessKeyDoesNotExist { public_key: public_key.clone() }
        })
    }

    pub fn view_access_keys(
        &self,
        state_update: &TrieUpdate,
        account_id: &AccountId,
    ) -> Result<Vec<(PublicKey, AccessKey)>, errors::ViewAccessKeyError> {
        let prefix = trie_key_parsers::get_raw_prefix_for_access_keys(account_id);
        let raw_prefix: &[u8] = prefix.as_ref();
        let access_keys =
            state_update
                .iter(&prefix)?
                .map(|key| {
                    let key = key?;
                    let public_key = &key[raw_prefix.len()..];
                    let access_key = unc_store::get_access_key_raw(state_update, &key)?
                        .ok_or_else(|| errors::ViewAccessKeyError::InternalError {
                            error_message: "Unexpected missing key from iterator".to_string(),
                        })?;
                    PublicKey::try_from_slice(public_key)
                        .map_err(|_| errors::ViewAccessKeyError::InternalError {
                            error_message: format!(
                                "Unexpected invalid public key {:?} received from store",
                                public_key
                            ),
                        })
                        .map(|key| (key, access_key))
                })
                .collect::<Result<Vec<_>, errors::ViewAccessKeyError>>();
        access_keys
    }

    #[allow(deprecated)]
    pub fn view_chip_list(
        &self,
        state_update: &TrieUpdate,
        account_id: &AccountId,
    ) -> Result<Vec<ChipView>, ViewChipError> {
        let prefix = trie_key_parsers::get_raw_prefix_for_rsa_keys(account_id);
        let raw_prefix: &[u8] = prefix.as_ref();
        let mut chip_views = Vec::new();

        let iter_result = state_update
            .iter(&prefix)
            .map_err(|_| ViewChipError::InternalError {
                error_message: "Failed to iterate over state_update".to_string(),
            })?;

        for key_result in iter_result {
            let key = key_result.map_err(|_| ViewChipError::InternalError {
                error_message: "Iteration error encountered".to_string(),
            })?;

            let public_key_str = &key[raw_prefix.len()..];

            let public_key = PublicKey::try_from_slice(public_key_str)
                .map_err(|_| errors::ViewChipError::InternalError {
                    error_message: format!(
                        "Unexpected invalid public key {:?} received from store",
                        public_key_str
                    ),
                })?;
            // Extract the part of the key that follows the prefix, if needed

            let chip_action = unc_store::get_rsa2048_keys_raw(state_update, &key).map_err(|e| {
                ViewChipError::InternalError {
                    error_message: format!("Storage error encountered: {:?}", e),
                }
            })?
                .ok_or_else(|| ViewChipError::InternalError {
                    error_message: "Unexpected missing key from iterator".to_string(),
                })?;

            match serde_json::from_slice::<serde_json::Value>(&chip_action.args) {
                Ok(parsed_args) => {
                    let mut chip_view = ChipView {
                        miner_id: String::new(),
                        public_key: String::new(), // Assume initially empty, update if necessary
                        power: 0,
                        sn: String::new(),
                        bus_id: String::new(),
                        p2key: String::new(),
                    };

                    // Directly assign 'power'
                    // if let Some(power_val) = parsed_args.get("power").and_then(|v| v.as_u64()) {
                    //     chip_view.power = power_val;
                    // }
                    // Handle power field with dual-path parsing
                    if let Some(power_val) = parsed_args.get("power") {
                        if let Some(power_str) = power_val.as_str() {
                            chip_view.power = power_str.parse::<u64>().unwrap_or(0);
                        } else if let Some(power_number) = power_val.as_u64() {
                            chip_view.power = power_number;
                        } else {
                            println!("Power value is not a string or a number that fits into u64");
                        }
                    }

                    chip_view.public_key = public_key.to_string();

                    // Extract 'sn' directly
                    if let Some(sn_val) = parsed_args.get("sn").and_then(|v| v.as_str()) {
                        chip_view.sn = sn_val.to_string();
                    }

                    // Extract 'public_key' directly
                    if let Some(public_key_val) = parsed_args.get("public_key").and_then(|v| v.as_str()) {
                        chip_view.public_key = public_key_val.to_string();
                    }

                    // Extract 'miner_id' directly
                    if let Some(miner_id_val) = parsed_args.get("miner_id").and_then(|v| v.as_str()) {
                        chip_view.miner_id = miner_id_val.to_string();
                    }

                    // Extract 'bus_id' directly
                    if let Some(bus_id_val) = parsed_args.get("bus_id").and_then(|v| v.as_str()) {
                        chip_view.bus_id = bus_id_val.to_string();
                    }

                    // Extract 'p2key' directly
                    if let Some(p2key_val) = parsed_args.get("p2key").and_then(|v| v.as_str()) {
                        chip_view.p2key = p2key_val.to_string();
                    }

                    // Example: Update public_key or other fields based on key_suffix if applicable
                    // chip_view.public_key = String::from_utf8_lossy(key_suffix).to_string();

                    // Continue to extract and assign other fields as needed

                    chip_views.push(chip_view);
                }
                Err(_) => {
                    // Handle parsing error
                    return Err(ViewChipError::InternalError {
                        error_message: "Failed to parse JSON from args".to_string(),
                    });
                }
            }
        }

        Ok(chip_views)
    }



    pub fn view_state(
        &self,
        state_update: &TrieUpdate,
        account_id: &AccountId,
        prefix: &[u8],
        include_proof: bool,
    ) -> Result<ViewStateResult, errors::ViewStateError> {
        match get_account(state_update, account_id)? {
            Some(account) => {
                let code_len = get_code(state_update, account_id, Some(account.code_hash()))?
                    .map(|c| c.code().len() as u64)
                    .unwrap_or_default();
                if let Some(limit) = self.state_size_limit {
                    if account.storage_usage().saturating_sub(code_len) > limit {
                        return Err(errors::ViewStateError::AccountStateTooLarge {
                            requested_account_id: account_id.clone(),
                        });
                    }
                }
            }
            None => {
                return Err(errors::ViewStateError::AccountDoesNotExist {
                    requested_account_id: account_id.clone(),
                })
            }
        };

        let mut values = vec![];
        let query = trie_key_parsers::get_raw_prefix_for_contract_data(account_id, prefix);
        let acc_sep_len = query.len() - prefix.len();
        let mut iter = state_update.trie().iter()?;
        iter.remember_visited_nodes(include_proof);
        iter.seek_prefix(&query)?;
        for item in &mut iter {
            let (key, value) = item?;
            values.push(StateItem { key: key[acc_sep_len..].to_vec().into(), value: value.into() });
        }
        let proof = iter.into_visited_nodes();
        Ok(ViewStateResult { values, proof })
    }

    pub fn call_function(
        &self,
        mut state_update: TrieUpdate,
        view_state: ViewApplyState,
        contract_id: &AccountId,
        method_name: &str,
        args: &[u8],
        logs: &mut Vec<String>,
        epoch_info_provider: &dyn EpochInfoProvider,
    ) -> Result<Vec<u8>, errors::CallFunctionError> {
        let now = Instant::now();
        let root = *state_update.get_root();
        let mut account = get_account(&state_update, contract_id)?.ok_or_else(|| {
            errors::CallFunctionError::AccountDoesNotExist {
                requested_account_id: contract_id.clone(),
            }
        })?;
        // TODO(#1015): Add ability to pass public key and originator_id
        let originator_id = contract_id;
        let public_key = PublicKey::empty(KeyType::ED25519);
        let empty_hash = CryptoHash::default();
        let mut receipt_manager = ReceiptManager::default();
        let mut runtime_ext = RuntimeExt::new(
            &mut state_update,
            &mut receipt_manager,
            contract_id,
            &empty_hash,
            &view_state.epoch_id,
            &view_state.prev_block_hash,
            &view_state.block_hash,
            epoch_info_provider,
            view_state.current_protocol_version,
        );
        let config_store = RuntimeConfigStore::new(None);
        let config = config_store.get_config(PROTOCOL_VERSION);
        let apply_state = ApplyState {
            block_height: view_state.block_height,
            // Used for legacy reasons
            prev_block_hash: view_state.prev_block_hash,
            block_hash: view_state.block_hash,
            epoch_id: view_state.epoch_id.clone(),
            epoch_height: view_state.epoch_height,
            gas_price: 0,
            block_timestamp: view_state.block_timestamp,
            gas_limit: None,
            random_seed: root,
            current_protocol_version: view_state.current_protocol_version,
            config: config.clone(),
            cache: view_state.cache,
            is_new_chunk: false,
            migration_data: Arc::new(MigrationData::default()),
            migration_flags: MigrationFlags::default(),
        };
        let action_receipt = ActionReceipt {
            signer_id: originator_id.clone(),
            signer_public_key: public_key,
            gas_price: 0,
            output_data_receivers: vec![],
            input_data_ids: vec![],
            actions: vec![],
        };
        let function_call = FunctionCallAction {
            method_name: method_name.to_string(),
            args: args.to_vec(),
            gas: self.max_gas_burnt_view,
            deposit: 0,
        };
        let outcome = execute_function_call(
            &apply_state,
            &mut runtime_ext,
            &mut account,
            originator_id,
            &action_receipt,
            &[],
            &function_call,
            &empty_hash,
            config,
            true,
            Some(ViewConfig { max_gas_burnt: self.max_gas_burnt_view }),
        )
        .map_err(|e| errors::CallFunctionError::InternalError { error_message: e.to_string() })?;
        let elapsed = now.elapsed();
        let time_ms =
            (elapsed.as_secs() as f64 / 1_000.0) + f64::from(elapsed.subsec_nanos()) / 1_000_000.0;
        let time_str = format!("{:.*}ms", 2, time_ms);

        if let Some(err) = outcome.aborted {
            logs.extend(outcome.logs);
            let message = format!("wasm execution failed with error: {:?}", err);
            debug!(target: "runtime", "(exec time {}) {}", time_str, message);
            Err(errors::CallFunctionError::VMError { error_message: message })
        } else {
            debug!(target: "runtime", "(exec time {}) result of execution: {:?}", time_str, outcome);
            logs.extend(outcome.logs);
            let result = match outcome.return_data {
                ReturnData::Value(buf) => buf,
                ReturnData::ReceiptIndex(_) | ReturnData::None => vec![],
            };
            Ok(result)
        }
    }
}

// Helper function to deserialize ChipView from binary format
#[allow(dead_code)]
fn deserialize_chip_view(encoded: &[u8]) -> Result<ChipView, Box<dyn std::error::Error>> {
    // Directly deserialize the JSON data into ChipView
    let chip_view = serde_json::from_slice::<ChipView>(encoded)?;
    Ok(chip_view)
}

