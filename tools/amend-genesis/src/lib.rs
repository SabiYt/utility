use anyhow::Context;

use unc_chain_configs::{Genesis, GenesisValidationMode};
use unc_crypto::PublicKey;
use unc_primitives::hash::CryptoHash;
use unc_primitives::shard_layout::ShardLayout;
use unc_primitives::state_record::StateRecord;
use unc_primitives::types::{AccountId, AccountInfo};
use unc_primitives::utils;
use unc_primitives::version::ProtocolVersion;
use unc_primitives_core::account::{AccessKey, Account};
use unc_primitives_core::types::{Balance, BlockHeightDelta, NumBlocks, NumSeats, NumShards, Power};
use num_rational::Rational32;
use serde::ser::{SerializeSeq, Serializer};
use std::collections::{hash_map, HashMap};
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

mod cli;

pub use cli::AmendGenesisCommand;

// while parsing the --extra-records file we will keep track of the records we see for each
// account here, and then at the end figure out what to put in the storage_usage field
#[derive(Debug, Default)]
struct AccountRecords {
    account: Option<Account>,
    // when we parse the validators file, we will set the balance in the account to 0
    // and set this to true so we remember later to set some default value, and if we
    // end up seeing the account listed in the input records file, we'll use the total
    // given there
    amount_needed: bool,
    keys: HashMap<PublicKey, AccessKey>,
    // code state records must appear after the account state record. So for accounts we're
    // modifying/adding keys for, we will remember any code records (there really should only be one),
    // and add them to the output only after we write the account record
    extra_records: Vec<StateRecord>,
}

// set the total balance to what's in src, keeping the pledging amount the same
fn set_total_balance(dst: &mut Account, src: &Account) {
    let total = src.amount() + src.pledging();
    if total > dst.pledging() {
        dst.set_amount(total - dst.pledging());
    }
}

impl AccountRecords {
    fn new(amount: Balance, pledging: Balance, power: Power, num_bytes_account: u64) -> Self {
        let mut ret = Self::default();
        ret.set_account(amount, pledging, power, num_bytes_account);
        ret
    }

    fn new_validator(amount: Balance, power: Power, pledge: Balance, num_bytes_account: u64) -> Self {
        let mut ret = Self::default();
        ret.set_account(amount, pledge, power, num_bytes_account);
        ret.amount_needed = true;
        ret
    }

    fn set_account(&mut self, amount: Balance, pledging: Balance, power: Power, num_bytes_account: u64) {
        assert!(self.account.is_none());
        let account = Account::new(amount, pledging, power, CryptoHash::default(), num_bytes_account);
        self.account = Some(account);
    }

    fn update_from_existing(&mut self, existing: &Account) {
        match &mut self.account {
            Some(account) => {
                // an account added in extra_records (or one of the validators) also exists in the original
                // records. Set the storage usage to reflect whatever's in the original records, and at the
                // end we will add to the storage usage with any extra keys added for this account
                account.set_storage_usage(existing.storage_usage());
                account.set_code_hash(existing.code_hash());
                account.set_power(existing.power());
                if self.amount_needed {
                    set_total_balance(account, existing);
                }
            }
            None => {
                let mut account = existing.clone();
                account.set_amount(account.amount() + account.pledging());
                account.set_pledging(0);
                account.set_power(0);
                self.account = Some(account);
            }
        }
        self.amount_needed = false;
    }

    fn push_extra_record(&mut self, record: StateRecord) {
        self.extra_records.push(record);
    }

    fn write_out<S: SerializeSeq>(
        self,
        account_id: AccountId,
        seq: &mut S,
        total_supply: &mut Balance,
        num_extra_bytes_record: u64,
    ) -> anyhow::Result<()>
    where
        <S as SerializeSeq>::Error: Send + Sync + 'static,
    {
        match self.account {
            Some(mut account) => {
                for (public_key, access_key) in self.keys {
                    let storage_usage = account.storage_usage()
                        + public_key.len() as u64
                        + borsh::object_length(&access_key).unwrap() as u64
                        + num_extra_bytes_record;
                    account.set_storage_usage(storage_usage);

                    seq.serialize_element(&StateRecord::AccessKey {
                        account_id: account_id.clone(),
                        public_key,
                        access_key,
                    })?;
                }
                if self.amount_needed {
                    account.set_amount(10_000 * framework::config::UNC_BASE);
                }
                *total_supply += account.amount() + account.pledging();
                seq.serialize_element(&StateRecord::Account { account_id, account })?;
                for record in self.extra_records.iter() {
                    seq.serialize_element(record)?;
                }
            }
            None => {
                tracing::warn!("access keys for {} were included in --extra-records, but no Account record was found. Not adding them to the output", &account_id);
            }
        }
        Ok(())
    }
}

fn validator_records(
    validators: &[AccountInfo],
    num_bytes_account: u64,
) -> anyhow::Result<HashMap<AccountId, AccountRecords>> {
    let mut records = HashMap::new();
    for AccountInfo { account_id, public_key, pledging, power } in validators.iter() {
        let mut r: AccountRecords = AccountRecords::new_validator(*pledging,  *power, *pledging, num_bytes_account);
        r.keys.insert(public_key.clone(), AccessKey::full_access());
        if records.insert(account_id.clone(), r).is_some() {
            anyhow::bail!("validator {} specified twice", account_id);
        }
    }
    Ok(records)
}

fn parse_validators(path: &Path) -> anyhow::Result<Vec<AccountInfo>> {
    let validators = std::fs::read_to_string(path)
        .with_context(|| format!("failed reading from {}", path.display()))?;
    let validators = serde_json::from_str(&validators)
        .with_context(|| format!("failed deserializing from {}", path.display()))?;
    Ok(validators)
}

fn parse_extra_records(
    records_file: &Path,
    num_bytes_account: u64,
) -> anyhow::Result<HashMap<AccountId, AccountRecords>> {
    let reader =
        BufReader::new(File::open(records_file).with_context(|| {
            format!("Failed opening validators file {}", records_file.display())
        })?);
    let mut records = HashMap::new();

    let mut result = Ok(());
    unc_chain_configs::stream_records_from_file(reader, |r| {
        match r {
            StateRecord::Account { account_id, account } => {
                if account.code_hash() != CryptoHash::default() {
                    result = Err(anyhow::anyhow!(
                        "FIXME: accounts in --extra-records with code_hash set not supported"
                    ));
                }
                match records.entry(account_id.clone()) {
                    hash_map::Entry::Vacant(e) => {
                        let r = AccountRecords::new(
                            account.amount(),
                            account.pledging(),
                            account.power(),
                            num_bytes_account,
                        );
                        e.insert(r);
                    }
                    hash_map::Entry::Occupied(mut e) => {
                        let r = e.get_mut();

                        if r.account.is_some() {
                            result = Err(anyhow::anyhow!(
                                "account {} given twice in extra records",
                                &account_id
                            ));
                        }
                        r.set_account(account.amount(), account.pledging(), account.power(), num_bytes_account);
                    }
                }
            }
            StateRecord::AccessKey { account_id, public_key, access_key } => {
                records.entry(account_id).or_default().keys.insert(public_key, access_key);
            }
            _ => {
                result = Err(anyhow::anyhow!(
                    "FIXME: only Account and AccessKey records are supported in --extra-records"
                ));
            }
        };
    })
    .context("Failed deserializing records from --extra-records")?;

    Ok(records)
}

fn wanted_records(
    validators: &[AccountInfo],
    extra_records: Option<&Path>,
    num_bytes_account: u64,
) -> anyhow::Result<HashMap<AccountId, AccountRecords>> {
    let mut records = validator_records(validators, num_bytes_account)?;

    if let Some(path) = extra_records {
        let extra = parse_extra_records(path, num_bytes_account)?;

        for (account_id, account_records) in extra {
            match records.entry(account_id) {
                hash_map::Entry::Occupied(mut e) => {
                    let validator_records = e.get_mut();

                    if let Some(account) = &account_records.account {
                        set_total_balance(validator_records.account.as_mut().unwrap(), account);
                        validator_records.amount_needed = false;
                    }
                    validator_records.keys.extend(account_records.keys);
                }
                hash_map::Entry::Vacant(e) => {
                    e.insert(account_records);
                }
            }
        }
    }

    Ok(records)
}

#[derive(Default)]
pub struct GenesisChanges {
    pub chain_id: Option<String>,
    pub protocol_version: Option<ProtocolVersion>,
    pub num_seats: Option<NumSeats>,
    pub epoch_length: Option<BlockHeightDelta>,
    pub transaction_validity_period: Option<NumBlocks>,
    pub protocol_reward_rate: Option<Rational32>,
    pub block_producer_kickout_threshold: Option<u8>,
    pub chunk_producer_kickout_threshold: Option<u8>,
    pub min_gas_price: Option<Balance>,
    pub max_gas_price: Option<Balance>,
}

/// Amend a genesis/records file created by `dump-state`.
pub fn amend_genesis(
    genesis_file_in: &Path,
    genesis_file_out: &Path,
    records_file_in: &Path,
    records_file_out: &Path,
    extra_records: Option<&Path>,
    validators: &Path,
    shard_layout_file: Option<&Path>,
    genesis_changes: &GenesisChanges,
    num_bytes_account: u64,
    num_extra_bytes_record: u64,
) -> anyhow::Result<()> {
    let mut genesis = Genesis::from_file(genesis_file_in, GenesisValidationMode::UnsafeFast)?;

    let shard_layout = if let Some(path) = shard_layout_file {
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("failed reading shard layout file {}", path.display()))?;
        Some(
            serde_json::from_str::<ShardLayout>(&s)
                .context("failed deserializing --shard-layout-file")?,
        )
    } else {
        None
    };

    let reader = BufReader::new(File::open(records_file_in).with_context(|| {
        format!("Failed opening input records file {}", records_file_in.display())
    })?);
    let records_out = BufWriter::new(File::create(records_file_out).with_context(|| {
        format!("Failed opening output records file {}", records_file_out.display())
    })?);
    let mut records_ser = serde_json::Serializer::new(records_out);
    let mut records_seq = records_ser.serialize_seq(None).unwrap();

    let validators = parse_validators(validators)?;
    let mut wanted = wanted_records(&validators, extra_records, num_bytes_account)?;
    let mut total_supply = 0;

    unc_chain_configs::stream_records_from_file(reader, |mut r| {
        match &mut r {
            StateRecord::AccessKey { account_id, public_key, access_key } => {
                if let Some(a) = wanted.get_mut(account_id) {
                    if let Some(a) = a.keys.remove(public_key) {
                        *access_key = a;
                    }
                }
                records_seq.serialize_element(&r).unwrap();
            }
            StateRecord::Account { account_id, account } => {
                if let Some(acc) = wanted.get_mut(account_id) {
                    acc.update_from_existing(account);
                } else {
                    if account.pledging() != 0 {
                        account.set_amount(account.amount() + account.pledging());
                        account.set_pledging(0);
                    }
                    total_supply += account.amount() + account.pledging();
                    records_seq.serialize_element(&r).unwrap();
                }
            }
            StateRecord::Contract { account_id, .. } => {
                if let Some(records) = wanted.get_mut(account_id) {
                    records.push_extra_record(r);
                } else {
                    records_seq.serialize_element(&r).unwrap();
                }
            }
            _ => {
                records_seq.serialize_element(&r).unwrap();
            }
        };
    })?;

    for (account_id, records) in wanted {
        records.write_out(
            account_id,
            &mut records_seq,
            &mut total_supply,
            num_extra_bytes_record,
        )?;
    }

    genesis.config.total_supply = total_supply;
    // TODO: give an option to set this
    genesis.config.num_block_producer_seats = validators.len() as NumSeats;
    // here we have already checked that there are no duplicate validators in wanted_records()
    genesis.config.validators = validators;
    if let Some(chain_id) = &genesis_changes.chain_id {
        genesis.config.chain_id = chain_id.clone();
    }
    if let Some(n) = genesis_changes.num_seats {
        genesis.config.num_block_producer_seats = n;
    }
    if let Some(shard_layout) = shard_layout {
        genesis.config.avg_hidden_validator_seats_per_shard =
            shard_layout.shard_ids().into_iter().map(|_| 0).collect();
        genesis.config.num_block_producer_seats_per_shard = utils::get_num_seats_per_shard(
            shard_layout.shard_ids().count() as NumShards,
            genesis.config.num_block_producer_seats,
        );
        genesis.config.shard_layout = shard_layout;
    }
    if let Some(v) = genesis_changes.protocol_version {
        genesis.config.protocol_version = v;
    }
    if let Some(l) = genesis_changes.epoch_length {
        genesis.config.epoch_length = l;
    }
    if let Some(t) = genesis_changes.transaction_validity_period {
        genesis.config.transaction_validity_period = t;
    }
    if let Some(r) = genesis_changes.protocol_reward_rate {
        genesis.config.protocol_reward_rate = r;
    }
    if let Some(t) = genesis_changes.block_producer_kickout_threshold {
        genesis.config.block_producer_kickout_threshold = t;
    }
    if let Some(t) = genesis_changes.chunk_producer_kickout_threshold {
        genesis.config.chunk_producer_kickout_threshold = t;
    }
    if let Some(p) = genesis_changes.min_gas_price {
        genesis.config.min_gas_price = p;
    }
    if let Some(p) = genesis_changes.max_gas_price {
        genesis.config.max_gas_price = p;
    }
    genesis.to_file(genesis_file_out);
    records_seq.end()?;
    Ok(())
}

#[cfg(test)]
mod test {
    use anyhow::Context;
    use unc_chain_configs::{get_initial_supply, Genesis, GenesisConfig};
    use unc_primitives::hash::CryptoHash;
    use unc_primitives::shard_layout::ShardLayout;
    use unc_primitives::state_record::StateRecord;
    use unc_primitives::static_clock::StaticClock;
    use unc_primitives::types::{AccountId, AccountInfo};
    use unc_primitives::utils;
    use unc_primitives::version::PROTOCOL_VERSION;
    use unc_primitives_core::account::{AccessKey, Account};
    use unc_primitives_core::types::{Balance, StorageUsage};
    use num_rational::Rational32;
    use std::collections::{HashMap, HashSet};
    use std::str::FromStr;
    use tempfile::NamedTempFile;

    // these (TestAccountInfo, TestStateRecord, and ParsedTestCase) are here so we can
    // have all static data in the testcases below
    struct TestAccountInfo {
        account_id: &'static str,
        public_key: &'static str,
        amount: Balance,
    }

    impl TestAccountInfo {
        fn parse(&self) -> AccountInfo {
            AccountInfo {
                account_id: self.account_id.parse().unwrap(),
                public_key: self.public_key.parse().unwrap(),
                pledging: self.amount,
                power: 0,
            }
        }
    }

    enum TestStateRecord {
        Account {
            account_id: &'static str,
            amount: Balance,
            pledging: Balance,
            /// Storage used by the given account, includes account id, this struct, access keys and other data.
            storage_usage: StorageUsage,
        },
        AccessKey {
            account_id: &'static str,
            public_key: &'static str,
        },
        Contract {
            account_id: &'static str,
        },
    }

    impl TestStateRecord {
        fn parse(&self) -> StateRecord {
            match &self {
                Self::Account { account_id, amount, pledging, storage_usage } => {
                    let account =
                        Account::new(*amount, *pledging, CryptoHash::default(), *storage_usage);
                    StateRecord::Account { account_id: account_id.parse().unwrap(), account }
                }
                Self::AccessKey { account_id, public_key } => StateRecord::AccessKey {
                    account_id: account_id.parse().unwrap(),
                    public_key: public_key.parse().unwrap(),
                    access_key: AccessKey::full_access(),
                },
                Self::Contract { account_id } => StateRecord::Contract {
                    account_id: account_id.parse().unwrap(),
                    code: vec![123],
                },
            }
        }
    }

    struct ParsedTestCase {
        genesis: Genesis,
        records_file_in: NamedTempFile,
        validators_in: Vec<AccountInfo>,
        extra_records: Vec<StateRecord>,
        wanted_records: Vec<StateRecord>,
    }

    struct TestCase {
        // for convenience, the validators set in the initial genesis file, matching
        // the accounts in records_in with nonzero `pledging`
        initial_validators: &'static [TestAccountInfo],
        // records to put in the --records-file-in file
        records_in: &'static [TestStateRecord],
        // account infos to put in the --validators file
        validators_in: &'static [TestAccountInfo],
        // records to put in the --extra-records file
        extra_records: &'static [TestStateRecord],
        // the records we want to appear in the output
        wanted_records: &'static [TestStateRecord],
    }

    fn compare_records(
        got_records: Vec<StateRecord>,
        wanted_records: Vec<StateRecord>,
    ) -> anyhow::Result<()> {
        let mut got_accounts = HashMap::new();
        let mut got_keys = HashSet::new();
        let mut got_contracts = HashMap::<AccountId, usize>::new();
        let mut wanted_accounts = HashMap::new();
        let mut wanted_keys = HashSet::new();
        let mut wanted_contracts = HashMap::<AccountId, usize>::new();

        for r in got_records {
            match r {
                StateRecord::Account { account_id, account } => {
                    if got_accounts
                        .insert(
                            account_id.clone(),
                            (
                                account.amount(),
                                account.pledging(),
                                account.code_hash(),
                                account.storage_usage(),
                            ),
                        )
                        .is_some()
                    {
                        anyhow::bail!("two account records in the output for {}", &account_id);
                    }
                }
                StateRecord::AccessKey { account_id, public_key, access_key } => {
                    if !got_keys.insert((account_id.clone(), public_key.clone(), access_key)) {
                        anyhow::bail!(
                            "two access key records in the output for {}, {}",
                            &account_id,
                            &public_key
                        );
                    }
                }
                StateRecord::Contract { account_id, .. } => {
                    if !got_accounts.contains_key(&account_id) {
                        anyhow::bail!(
                            "account {} has a code state record before the account state record",
                            &account_id
                        );
                    }
                    *got_contracts.entry(account_id).or_default() += 1;
                }
                _ => anyhow::bail!("got an unexpected record in the output: {}", r),
            };
        }
        for r in wanted_records {
            match r {
                StateRecord::Account { account_id, account } => {
                    wanted_accounts.insert(
                        account_id,
                        (
                            account.amount(),
                            account.pledging(),
                            account.code_hash(),
                            account.storage_usage(),
                        ),
                    );
                }
                StateRecord::AccessKey { account_id, public_key, access_key } => {
                    wanted_keys.insert((account_id, public_key, access_key));
                }
                StateRecord::Contract { account_id, .. } => {
                    *wanted_contracts.entry(account_id).or_default() += 1;
                }
                _ => anyhow::bail!("got an unexpected record in the output: {}", r),
            };
        }

        assert_eq!(got_accounts, wanted_accounts);
        assert_eq!(got_keys, wanted_keys);
        assert_eq!(got_contracts, wanted_contracts);
        Ok(())
    }

    impl TestCase {
        fn parse(&self) -> anyhow::Result<ParsedTestCase> {
            let initial_validators = self.initial_validators.iter().map(|v| v.parse()).collect();
            let records_in: Vec<_> = self.records_in.iter().map(|r| r.parse()).collect();

            let num_shards = 4;
            let shards = ShardLayout::v1(
                (0..num_shards - 1)
                    .map(|f| AccountId::from_str(format!("shard{}.test.unc", f).as_str()).unwrap())
                    .collect(),
                None,
                1,
            );

            let genesis_config = GenesisConfig {
                protocol_version: PROTOCOL_VERSION,
                genesis_time: StaticClock::utc(),
                chain_id: "rusttestnet".to_string(),
                genesis_height: 0,
                num_block_producer_seats: framework::config::NUM_BLOCK_PRODUCER_SEATS,
                num_block_producer_seats_per_shard: utils::get_num_seats_per_shard(
                    num_shards,
                    framework::config::NUM_BLOCK_PRODUCER_SEATS,
                ),
                avg_hidden_validator_seats_per_shard: (0..num_shards).map(|_| 0).collect(),
                dynamic_resharding: false,
                protocol_upgrade_pledge_threshold:
                    framework::config::PROTOCOL_UPGRADE_STAKE_THRESHOLD,
                epoch_length: 1000,
                gas_limit: framework::config::INITIAL_GAS_LIMIT,
                gas_price_adjustment_rate: framework::config::GAS_PRICE_ADJUSTMENT_RATE,
                block_producer_kickout_threshold:
                    framework::config::BLOCK_PRODUCER_KICKOUT_THRESHOLD,
                chunk_producer_kickout_threshold:
                    framework::config::CHUNK_PRODUCER_KICKOUT_THRESHOLD,
                online_max_threshold: Rational32::new(99, 100),
                online_min_threshold: Rational32::new(
                    framework::config::BLOCK_PRODUCER_KICKOUT_THRESHOLD as i32,
                    100,
                ),
                validators: initial_validators,
                transaction_validity_period: framework::config::TRANSACTION_VALIDITY_PERIOD,
                protocol_reward_rate: framework::config::PROTOCOL_REWARD_RATE,
                max_inflation_rate: framework::config::MAX_INFLATION_RATE,
                total_supply: get_initial_supply(&records_in),
                num_blocks_per_year: framework::config::NUM_BLOCKS_PER_YEAR,
                protocol_treasury_account: "treasury.unc".parse().unwrap(),
                fishermen_threshold: framework::config::FISHERMEN_THRESHOLD,
                shard_layout: shards,
                min_gas_price: framework::config::MIN_GAS_PRICE,
                ..Default::default()
            };

            let mut records_file_in =
                tempfile::NamedTempFile::new().context("failed creating tmp file")?;
            serde_json::to_writer(&mut records_file_in, &records_in)
                .context("failed writing to --records-file-in")?;
            let genesis = Genesis::new_with_path(genesis_config, records_file_in.path())?;

            Ok(ParsedTestCase {
                genesis,
                records_file_in,
                validators_in: self.validators_in.iter().map(|v| v.parse()).collect(),
                extra_records: self.extra_records.iter().map(|r| r.parse()).collect(),
                wanted_records: self.wanted_records.iter().map(|r| r.parse()).collect(),
            })
        }

        // take the records in the test case and write them to temp files, and then call amend_genesis() and
        // check that the resulting genesis and records files match what's in self.want_records
        // right now we aren't testing that other kinds of records appearing in the input records file
        // will make it into the output, but that part is pretty simple
        fn run(&self) -> anyhow::Result<()> {
            let ParsedTestCase {
                genesis,
                records_file_in,
                validators_in,
                extra_records,
                wanted_records,
            } = self.parse()?;

            let mut genesis_file_in =
                tempfile::NamedTempFile::new().context("failed creating tmp file")?;
            let mut validators_file =
                tempfile::NamedTempFile::new().context("failed creating tmp file")?;
            let mut extra_records_file =
                tempfile::NamedTempFile::new().context("failed creating tmp file")?;
            let genesis_file_out =
                tempfile::NamedTempFile::new().context("failed creating tmp file")?;
            let records_file_out =
                tempfile::NamedTempFile::new().context("failed creating tmp file")?;

            serde_json::to_writer(&mut validators_file, &validators_in)
                .context("failed writing to --validators")?;
            serde_json::to_writer(&mut extra_records_file, &extra_records)
                .context("failed writing to --extra-records")?;
            serde_json::to_writer(&mut genesis_file_in, &genesis)
                .context("failed writing to --genesis-file-in")?;

            crate::amend_genesis(
                genesis_file_in.path(),
                genesis_file_out.path(),
                records_file_in.path(),
                records_file_out.path(),
                Some(extra_records_file.path()),
                validators_file.path(),
                None,
                &crate::GenesisChanges::default(),
                100,
                40,
            )
            .context("amend_genesis() failed")?;

            let got_records = std::fs::read_to_string(records_file_out.path())
                .context("failed reading from --records-file-out")?;
            let got_records: Vec<StateRecord> = serde_json::from_str(&got_records)
                .context("failed deserializing --records-file-out")?;

            compare_records(got_records, wanted_records)
        }
    }

    static TEST_CASES: &[TestCase] = &[
        // first one adds one validator (foo2), bumps up another's balance (foo0), and adds an extra account (extra-account.unc)
        TestCase {
            initial_validators: &[
                TestAccountInfo {
                    account_id: "foo0",
                    public_key: "ed25519:He7QeRuwizNEhBioYG3u4DZ8jWXyETiyNzFD3MkTjDMf",
                    amount: 1_000_000,
                },
                TestAccountInfo {
                    account_id: "foo1",
                    public_key: "ed25519:FXXrTXiKWpXj1R6r5fBvMLpstd8gPyrBq3qMByqKVzKF",
                    amount: 2_000_000,
                },
            ],
            records_in: &[
                TestStateRecord::Account {
                    account_id: "foo0",
                    amount: 1_000_000,
                    pledging: 1_000_000,
                    storage_usage: 182,
                },
                TestStateRecord::AccessKey {
                    account_id: "foo0",
                    public_key: "ed25519:He7QeRuwizNEhBioYG3u4DZ8jWXyETiyNzFD3MkTjDMf",
                },
                TestStateRecord::Account {
                    account_id: "foo1",
                    amount: 1_000_000,
                    pledging: 2_000_000,
                    storage_usage: 182,
                },
                TestStateRecord::AccessKey {
                    account_id: "foo1",
                    public_key: "ed25519:FXXrTXiKWpXj1R6r5fBvMLpstd8gPyrBq3qMByqKVzKF",
                },
                TestStateRecord::Account {
                    account_id: "asdf.unc",
                    amount: 1_234_000,
                    pledging: 0,
                    storage_usage: 182,
                },
                TestStateRecord::AccessKey {
                    account_id: "asdf.unc",
                    public_key: "ed25519:5C66RSJgwK17Yb6VtTbgBCFHDRPzGUd6AAhFdXNvmJuo",
                },
            ],
            validators_in: &[
                TestAccountInfo {
                    account_id: "foo0",
                    public_key: "ed25519:He7QeRuwizNEhBioYG3u4DZ8jWXyETiyNzFD3MkTjDMf",
                    amount: 1_000_000,
                },
                TestAccountInfo {
                    account_id: "foo1",
                    public_key: "ed25519:FXXrTXiKWpXj1R6r5fBvMLpstd8gPyrBq3qMByqKVzKF",
                    amount: 2_000_000,
                },
                TestAccountInfo {
                    account_id: "foo2",
                    public_key: "ed25519:Eo9W44tRMwcYcoua11yM7Xfr1DjgR4EWQFM3RU27MEX8",
                    amount: 3_000_000,
                },
            ],
            extra_records: &[
                TestStateRecord::Account {
                    account_id: "foo0",
                    amount: 100_000_000,
                    pledging: 50_000_000,
                    storage_usage: 0,
                },
                TestStateRecord::Account {
                    account_id: "extra-account.unc",
                    amount: 9_000_000,
                    pledging: 0,
                    storage_usage: 0,
                },
                TestStateRecord::AccessKey {
                    account_id: "extra-account.unc",
                    public_key: "ed25519:BhnQV3oJa8iSQDKDc8gy36TsenaMFmv7qHvcnutuXj33",
                },
            ],
            wanted_records: &[
                TestStateRecord::Account {
                    account_id: "foo0",
                    amount: 149_000_000,
                    pledging: 1_000_000,
                    storage_usage: 182,
                },
                TestStateRecord::AccessKey {
                    account_id: "foo0",
                    public_key: "ed25519:He7QeRuwizNEhBioYG3u4DZ8jWXyETiyNzFD3MkTjDMf",
                },
                TestStateRecord::Account {
                    account_id: "foo1",
                    amount: 1_000_000,
                    pledging: 2_000_000,
                    storage_usage: 182,
                },
                TestStateRecord::AccessKey {
                    account_id: "foo1",
                    public_key: "ed25519:FXXrTXiKWpXj1R6r5fBvMLpstd8gPyrBq3qMByqKVzKF",
                },
                TestStateRecord::Account {
                    account_id: "foo2",
                    amount: 10_000 * framework::config::UNC_BASE,
                    pledging: 3_000_000,
                    storage_usage: 182,
                },
                TestStateRecord::AccessKey {
                    account_id: "foo2",
                    public_key: "ed25519:Eo9W44tRMwcYcoua11yM7Xfr1DjgR4EWQFM3RU27MEX8",
                },
                TestStateRecord::Account {
                    account_id: "asdf.unc",
                    amount: 1_234_000,
                    pledging: 0,
                    storage_usage: 182,
                },
                TestStateRecord::AccessKey {
                    account_id: "asdf.unc",
                    public_key: "ed25519:5C66RSJgwK17Yb6VtTbgBCFHDRPzGUd6AAhFdXNvmJuo",
                },
                TestStateRecord::Account {
                    account_id: "extra-account.unc",
                    amount: 9_000_000,
                    pledging: 0,
                    storage_usage: 182,
                },
                TestStateRecord::AccessKey {
                    account_id: "extra-account.unc",
                    public_key: "ed25519:BhnQV3oJa8iSQDKDc8gy36TsenaMFmv7qHvcnutuXj33",
                },
            ],
        },
        // this one changes the validator set completely, and adds an extra accounts and keys
        TestCase {
            initial_validators: &[
                TestAccountInfo {
                    account_id: "foo0",
                    public_key: "ed25519:He7QeRuwizNEhBioYG3u4DZ8jWXyETiyNzFD3MkTjDMf",
                    amount: 1_000_000,
                },
                TestAccountInfo {
                    account_id: "foo1",
                    public_key: "ed25519:FXXrTXiKWpXj1R6r5fBvMLpstd8gPyrBq3qMByqKVzKF",
                    amount: 2_000_000,
                },
            ],
            validators_in: &[
                TestAccountInfo {
                    account_id: "foo2",
                    public_key: "ed25519:He7QeRuwizNEhBioYG3u4DZ8jWXyETiyNzFD3MkTjDMf",
                    amount: 1_000_000,
                },
                TestAccountInfo {
                    account_id: "foo3",
                    public_key: "ed25519:FXXrTXiKWpXj1R6r5fBvMLpstd8gPyrBq3qMByqKVzKF",
                    amount: 2_000_000,
                },
            ],
            records_in: &[
                TestStateRecord::Account {
                    account_id: "foo0",
                    amount: 1_000_000,
                    pledging: 1_000_000,
                    storage_usage: 182,
                },
                TestStateRecord::AccessKey {
                    account_id: "foo0",
                    public_key: "ed25519:He7QeRuwizNEhBioYG3u4DZ8jWXyETiyNzFD3MkTjDMf",
                },
                TestStateRecord::Account {
                    account_id: "foo1",
                    amount: 1_000_000,
                    pledging: 2_000_000,
                    storage_usage: 182,
                },
                TestStateRecord::AccessKey {
                    account_id: "foo1",
                    public_key: "ed25519:FXXrTXiKWpXj1R6r5fBvMLpstd8gPyrBq3qMByqKVzKF",
                },
                TestStateRecord::Account {
                    account_id: "asdf.unc",
                    amount: 1_234_000,
                    pledging: 0,
                    storage_usage: 182,
                },
                TestStateRecord::AccessKey {
                    account_id: "asdf.unc",
                    public_key: "ed25519:5C66RSJgwK17Yb6VtTbgBCFHDRPzGUd6AAhFdXNvmJuo",
                },
            ],
            extra_records: &[
                TestStateRecord::Account {
                    account_id: "foo0",
                    amount: 100_000_000,
                    pledging: 0,
                    storage_usage: 0,
                },
                TestStateRecord::Account {
                    account_id: "foo2",
                    amount: 300_000_000,
                    pledging: 0,
                    storage_usage: 0,
                },
                TestStateRecord::AccessKey {
                    account_id: "foo0",
                    public_key: "ed25519:FXXrTXiKWpXj1R6r5fBvMLpstd8gPyrBq3qMByqKVzKF",
                },
                TestStateRecord::AccessKey {
                    account_id: "foo1",
                    public_key: "ed25519:He7QeRuwizNEhBioYG3u4DZ8jWXyETiyNzFD3MkTjDMf",
                },
                TestStateRecord::Account {
                    account_id: "extra-account.unc",
                    amount: 9_000_000,
                    pledging: 0,
                    storage_usage: 0,
                },
                TestStateRecord::AccessKey {
                    account_id: "extra-account.unc",
                    public_key: "ed25519:BhnQV3oJa8iSQDKDc8gy36TsenaMFmv7qHvcnutuXj33",
                },
            ],
            wanted_records: &[
                TestStateRecord::Account {
                    account_id: "foo0",
                    amount: 100_000_000,
                    pledging: 0,
                    storage_usage: 264,
                },
                TestStateRecord::AccessKey {
                    account_id: "foo0",
                    public_key: "ed25519:He7QeRuwizNEhBioYG3u4DZ8jWXyETiyNzFD3MkTjDMf",
                },
                TestStateRecord::AccessKey {
                    account_id: "foo0",
                    public_key: "ed25519:FXXrTXiKWpXj1R6r5fBvMLpstd8gPyrBq3qMByqKVzKF",
                },
                TestStateRecord::Account {
                    account_id: "foo1",
                    amount: 3_000_000,
                    pledging: 0,
                    storage_usage: 264,
                },
                TestStateRecord::AccessKey {
                    account_id: "foo1",
                    public_key: "ed25519:FXXrTXiKWpXj1R6r5fBvMLpstd8gPyrBq3qMByqKVzKF",
                },
                TestStateRecord::AccessKey {
                    account_id: "foo1",
                    public_key: "ed25519:He7QeRuwizNEhBioYG3u4DZ8jWXyETiyNzFD3MkTjDMf",
                },
                TestStateRecord::Account {
                    account_id: "foo2",
                    amount: 299_000_000,
                    pledging: 1_000_000,
                    storage_usage: 182,
                },
                TestStateRecord::AccessKey {
                    account_id: "foo2",
                    public_key: "ed25519:He7QeRuwizNEhBioYG3u4DZ8jWXyETiyNzFD3MkTjDMf",
                },
                TestStateRecord::Account {
                    account_id: "foo3",
                    amount: 10_000 * framework::config::UNC_BASE,
                    pledging: 2_000_000,
                    storage_usage: 182,
                },
                TestStateRecord::AccessKey {
                    account_id: "foo3",
                    public_key: "ed25519:FXXrTXiKWpXj1R6r5fBvMLpstd8gPyrBq3qMByqKVzKF",
                },
                TestStateRecord::Account {
                    account_id: "asdf.unc",
                    amount: 1_234_000,
                    pledging: 0,
                    storage_usage: 182,
                },
                TestStateRecord::AccessKey {
                    account_id: "asdf.unc",
                    public_key: "ed25519:5C66RSJgwK17Yb6VtTbgBCFHDRPzGUd6AAhFdXNvmJuo",
                },
                TestStateRecord::Account {
                    account_id: "extra-account.unc",
                    amount: 9_000_000,
                    pledging: 0,
                    storage_usage: 182,
                },
                TestStateRecord::AccessKey {
                    account_id: "extra-account.unc",
                    public_key: "ed25519:BhnQV3oJa8iSQDKDc8gy36TsenaMFmv7qHvcnutuXj33",
                },
            ],
        },
        // this one tests that account records appear before code records
        TestCase {
            initial_validators: &[TestAccountInfo {
                account_id: "foo0",
                public_key: "ed25519:He7QeRuwizNEhBioYG3u4DZ8jWXyETiyNzFD3MkTjDMf",
                amount: 1_000_000,
            }],
            validators_in: &[TestAccountInfo {
                account_id: "foo0",
                public_key: "ed25519:He7QeRuwizNEhBioYG3u4DZ8jWXyETiyNzFD3MkTjDMf",
                amount: 1_000_000,
            }],
            records_in: &[
                TestStateRecord::Account {
                    account_id: "foo0",
                    amount: 1_000_000,
                    pledging: 1_000_000,
                    storage_usage: 183,
                },
                TestStateRecord::AccessKey {
                    account_id: "foo0",
                    public_key: "ed25519:He7QeRuwizNEhBioYG3u4DZ8jWXyETiyNzFD3MkTjDMf",
                },
                TestStateRecord::Contract { account_id: "foo0" },
            ],
            extra_records: &[TestStateRecord::Account {
                account_id: "foo0",
                amount: 100_000_000,
                pledging: 0,
                storage_usage: 0,
            }],
            wanted_records: &[
                TestStateRecord::Account {
                    account_id: "foo0",
                    amount: 99_000_000,
                    pledging: 1_000_000,
                    storage_usage: 183,
                },
                TestStateRecord::AccessKey {
                    account_id: "foo0",
                    public_key: "ed25519:He7QeRuwizNEhBioYG3u4DZ8jWXyETiyNzFD3MkTjDMf",
                },
                TestStateRecord::Contract { account_id: "foo0" },
            ],
        },
    ];

    #[test]
    fn test_amend_genesis() {
        for t in TEST_CASES.iter() {
            t.run().unwrap();
        }
    }
}
