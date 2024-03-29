use cron_schedule::Schedule;
use near_sdk::{
    borsh::{self, BorshDeserialize, BorshSerialize},
    collections::{LookupMap, TreeMap, UnorderedMap},
    env,
    json_types::{Base64VecU8, ValidAccountId, U128, U64},
    log, near_bindgen,
    serde::{Deserialize, Serialize},
    serde_json::json,
    AccountId, Balance, BorshStorageKey, Gas, PanicOnDefault, Promise,
    StorageUsage,
    assert_one_yocto,
};
use std::str::FromStr;
pub use agent::Agent;
pub use tasks::Task;

mod agent;
mod owner;
mod storage_impl;
mod tasks;
mod utils;
mod views;

near_sdk::setup_alloc!();

// Balance & Fee Definitions
pub const ONE_NEAR: u128 = 1_000_000_000_000_000_000_000_000;
pub const GAS_BASE_PRICE: Balance = 100_000_000;
pub const GAS_BASE_FEE: Gas = 3_000_000_000_000;
// actual is: 13534954161128, higher in case treemap rebalance
pub const GAS_FOR_CALLBACK: Gas = 30_000_000_000_000;
pub const AGENT_BASE_FEE: Balance = 1_000_000_000_000_000_000_000; // 0.001 Ⓝ
pub const STAKE_BALANCE_MIN: u128 = 10 * ONE_NEAR;

// Boundary Definitions
pub const MAX_BLOCK_RANGE: u64 = 1_000_000_000_000_000;
pub const MAX_EPOCH_RANGE: u32 = 10_000;
pub const MAX_SECOND_RANGE: u32 = 600_000_000;
pub const SLOT_GRANULARITY: u64 = 60; // NOTE: Connection drain.. might be required if slot granularity changes
pub const NANO: u64 = 1_000_000_000;
pub const BPS_DENOMINATOR: u64 = 1_000;

#[derive(BorshStorageKey, BorshSerialize)]
pub enum StorageKeys {
    Tasks,
    Agents,
    Slots,
}

#[near_bindgen]
#[derive(BorshDeserialize, BorshSerialize, PanicOnDefault)]
pub struct Contract {
    // Runtime
    paused: bool,
    owner_id: AccountId,
    bps_block: [u64; 2],
    bps_timestamp: [u64; 2],

    // Basic management
    agents: LookupMap<AccountId, Agent>,
    slots: TreeMap<u128, Vec<Vec<u8>>>,
    tasks: UnorderedMap<Vec<u8>, Task>,

    // Economics
    available_balance: Balance,
    staked_balance: Balance,
    agent_fee: Balance,
    gas_price: Balance,
    proxy_callback_gas: Gas,
    slot_granularity: u64,

    // Storage
    agent_storage_usage: StorageUsage,
}

#[near_bindgen]
impl Contract {
    /// ```bash
    /// near call cron.testnet new --accountId cron.testnet
    /// ```
    #[init]
    pub fn new() -> Self {
        let mut this = Contract {
            paused: false,
            owner_id: env::signer_account_id(),
            bps_block: [env::block_index(), env::block_index()],
            bps_timestamp: [env::block_timestamp(), env::block_timestamp()],
            tasks: UnorderedMap::new(StorageKeys::Tasks),
            agents: LookupMap::new(StorageKeys::Agents),
            slots: TreeMap::new(StorageKeys::Slots),
            available_balance: 0,
            staked_balance: 0,
            agent_fee: AGENT_BASE_FEE,
            gas_price: GAS_BASE_PRICE,
            proxy_callback_gas: GAS_FOR_CALLBACK,
            slot_granularity: SLOT_GRANULARITY,
            agent_storage_usage: 0,
        };
        this.measure_account_storage_usage();
        this
    }

    /// Measure the storage an agent will take and need to provide
    fn measure_account_storage_usage(&mut self) {
        let initial_storage_usage = env::storage_usage();
        // Create a temporary, dummy entry and measure the storage used.
        let tmp_account_id = "a".repeat(64);
        let tmp_agent = Agent {
            payable_account_id: tmp_account_id.clone(),
            balance: U128::from(0),
            total_tasks_executed: U128::from(0),
        };
        self.agents.insert(&tmp_account_id, &tmp_agent);
        self.agent_storage_usage = env::storage_usage() - initial_storage_usage;
        // Remove the temporary entry.
        self.agents.remove(&tmp_account_id);
    }

    /// Takes an optional `offset`: the number of blocks to offset from now (current block height)
    /// If no offset, returns current slot based on current block height
    /// If offset, returns next slot based on current block height & integer offset
    /// rounded to nearest granularity (~every 1.6 block per sec)
    fn get_slot_id(&self, offset: Option<u64>) -> u128 {
        let current_block = env::block_index();
        let slot_id: u64 = if let Some(o) = offset {
            // NOTE: Assumption here is that the offset will be in seconds. (blocks per second)
            //       Slot granularity will be in minutes (60 blocks per slot)

            let slot_remainder = core::cmp::max(o % self.slot_granularity, 1);
            let slot_round =
                core::cmp::max(o.saturating_sub(slot_remainder), self.slot_granularity);
            let next = current_block + slot_round;

            // Protect against extreme future block schedules
            if next - current_block > current_block + MAX_BLOCK_RANGE {
                u64::min(next, current_block + MAX_BLOCK_RANGE)
            } else {
                next
            }
        } else {
            current_block
        };

        let slot_remainder = slot_id % self.slot_granularity;
        let slot_id_round = slot_id.saturating_sub(slot_remainder);

        u128::from(slot_id_round)
    }

    /// Parse cadence into a schedule
    /// Get next approximate block from a schedule
    /// return slot from the difference of upcoming block and current block
    fn get_slot_from_cadence(&self, cadence: String) -> u128 {
        let current_block = env::block_index();
        let current_block_ts = env::block_timestamp();

        // Schedule params
        // NOTE: eventually use TryFrom
        let schedule = Schedule::from_str(&cadence).unwrap();
        let next_ts = schedule.next_after(&current_block_ts).unwrap();
        let next_diff = next_ts - current_block_ts;

        // calculate the average blocks, to get predicted future block
        // Get the range of blocks for which we're taking the average
        // Remember `bps_block` is updated after every call to `tick`
        let blocks_total = core::cmp::max(current_block - self.bps_block[1], 1);
        // Generally, avoiding floats can be useful, here we set a denominator
        // Since the `bps` timestamp is in nanoseconds, we multiply the
        // numerator to match the magnitude
        // We use the `max` value to avoid division by 0
        let bps = ((blocks_total * NANO * BPS_DENOMINATOR)
            / std::cmp::max(current_block_ts - self.bps_timestamp[1], 1)).max(1);

        /*
        seconds * nano      blocks           1
         ---             *  ---         *   ---   = blocks offset (with extra 1000 magnitude)
          1             seconds * 1000      1000
        */
        let offset =
            ((next_diff as u128 * bps as u128) / BPS_DENOMINATOR as u128 / NANO as u128) as u64;
        let current = self.get_slot_id(None);
        let next_slot = self.get_slot_id(Some(offset));

        if current == next_slot {
            // Add slot granularity to make sure the minimum next slot is a block within next slot granularity range
            current + u128::from(self.slot_granularity)
        } else {
            next_slot
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use near_sdk::json_types::ValidAccountId;
    use near_sdk::test_utils::{accounts, VMContextBuilder};
    use near_sdk::{testing_env, MockedBlockchain};

    const BLOCK_START_BLOCK: u64 = 52_201_040;
    const BLOCK_START_TS: u64 = 1_624_151_503_447_000_000;

    fn get_context(predecessor_account_id: ValidAccountId) -> VMContextBuilder {
        let mut builder = VMContextBuilder::new();
        builder
            .current_account_id(accounts(0))
            .signer_account_id(predecessor_account_id.clone())
            .signer_account_pk(b"ed25519:4ZhGmuKTfQn9ZpHCQVRwEr4JnutL8Uu3kArfxEqksfVM".to_vec())
            .predecessor_account_id(predecessor_account_id)
            .block_index(BLOCK_START_BLOCK)
            .block_timestamp(BLOCK_START_TS);
        builder
    }

    #[test]
    fn test_contract_new() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let contract = Contract::new();
        testing_env!(context.is_view(true).build());
        assert!(contract.get_all_tasks(None).is_empty());
    }
}
