use crate::*;

#[derive(BorshDeserialize, BorshSerialize, Debug, Serialize, Deserialize, PartialEq)]
#[serde(crate = "near_sdk::serde")]
pub struct Task {
    /// Entity responsible for this task, can change task details
    pub owner_id: AccountId,

    /// Account to direct all execution calls against
    pub contract_id: AccountId,

    /// Contract method this task will be executing
    pub function_id: String,

    /// Crontab Spec String
    /// Defines the interval spacing of execution
    pub cadence: String,

    /// Defines if this task can continue until balance runs out
    pub recurring: bool,

    /// Total balance of NEAR available for current and future executions
    pub total_deposit: U128,

    /// Configuration of NEAR balance to send to each function call. This is the "amount" for a function call.
    pub deposit: U128,

    /// Configuration of NEAR balance to attach to each function call. This is the "gas" for a function call.
    pub gas: Gas,

    // NOTE: Only allow static pre-defined bytes
    pub arguments: Base64VecU8,
}

#[near_bindgen]
impl Contract {
    /// Allows any user or contract to pay for future txns based on a specific schedule
    /// contract, function id & other settings. When the task runs out of balance
    /// the task is no longer executed, any additional funds will be returned to task owner.
    ///
    /// ```bash
    /// near call cron.testnet create_task '{"contract_id": "counter.in.testnet","function_id": "increment","cadence": "0 0 */1 * * *","recurring": true,"deposit": 0,"gas": 2400000000000}' --accountId YOU.testnet
    /// ```
    #[payable]
    pub fn create_task(
        &mut self,
        contract_id: ValidAccountId,
        function_id: String,
        cadence: String,
        recurring: Option<bool>,
        deposit: Option<U128>,
        gas: Option<Gas>,
        arguments: Option<Base64VecU8>,
    ) -> Base64VecU8 {
        // No adding tasks while contract is paused
        assert_eq!(self.paused, false, "Create task paused");
        // check cadence can be parsed
        assert!(
            self.validate_cadence(&cadence),
            "Cadence string invalid"
        );

        let item = Task {
            owner_id: env::predecessor_account_id(),
            contract_id: contract_id.into(),
            function_id,
            cadence,
            recurring: recurring.unwrap_or(false),
            total_deposit: U128::from(env::attached_deposit()),
            deposit: U128::from(deposit.map(|v| v.0).unwrap_or(0u128)),
            gas: gas.unwrap_or(GAS_BASE_FEE),
            arguments: arguments.unwrap_or_else(|| Base64VecU8::from(vec![])),
        };

        // Check that balance is sufficient for 1 execution minimum
        let call_balance_used = self.task_balance_uses(&item);
        let min_balance_needed: u128 = if recurring == Some(true) {
            call_balance_used * 2
        } else {
            call_balance_used
        };
        // Agent fee is now too high for this check to matter
        // assert!(
        //     min_balance_needed > u128::from(GAS_BASE_FEE),
        //     "Gas minimum has not been met, need at least {}",
        //     min_balance_needed
        // );
        assert!(
            min_balance_needed <= item.total_deposit.0,
            "Not enough task balance to execute job, need at least {}",
            min_balance_needed
        );

        let hash = self.hash(&item);
        // log!("Task Hash (as bytes) {:?}", &hash);

        // Parse cadence into a future timestamp, then convert to a slot
        let next_slot = self.get_slot_from_cadence(item.cadence.clone());

        // Add task to catalog
        assert!(self.tasks.insert(&hash, &item).is_none(), "Task already exists");

        // Get previous task hashes in slot, add as needed
        let mut slot_slots = self.slots.get(&next_slot).unwrap_or(Vec::new());
        slot_slots.push(hash.clone());
        log!("Task next slot: {}", next_slot);
        self.slots.insert(&next_slot, &slot_slots);

        Base64VecU8::from(hash)
    }

    /// Deletes a task in its entirety, returning any remaining balance to task owner.
    ///
    /// ```bash
    /// near call cron.testnet remove_task '{"task_hash": ""}' --accountId YOU.testnet
    /// ```
    pub fn remove_task(&mut self, task_hash: Base64VecU8) {
        let hash = task_hash.0;
        let task = self.tasks.get(&hash).expect("No task found by hash");

        assert_eq!(
            task.owner_id,
            env::predecessor_account_id(),
            "Only owner can remove their task."
        );

        // If owner, allow to remove task
        self.exit_task(hash);
    }

    /// Internal management of finishing a task.
    /// Responsible for cleaning up storage &
    /// returning any remaining balance to task owner.
    fn exit_task(&mut self, task_hash: Vec<u8>) {
        let task = self.tasks.remove(&task_hash).expect("No task found by hash");

        // return any balance
        if task.total_deposit.0 > 0 {
            Promise::new(task.owner_id.to_string()).transfer(task.total_deposit.0);
        }

        // Remove task from schedule
        // Get previous task hashes in slot, find index of task hash, remove
        let next_slot = self.get_slot_from_cadence(task.cadence.clone());
        let mut slot_tasks = self.slots.get(&next_slot).unwrap_or(Vec::new());
        if slot_tasks.len() != 0 {
            slot_tasks.retain(|h| h != &task_hash);
            self.slots.insert(&next_slot, &slot_tasks);
        }
    }

    /// Executes a task based on the current task slot
    /// Computes whether a task should continue further or not
    /// Makes a cross-contract call with the task configuration
    /// Called directly by a registered agent
    ///
    /// ```bash
    /// near call cron.testnet proxy_call --accountId YOU.testnet
    /// ```
    // Questions:
    // Can the call fail and second promise continue?
    pub fn proxy_call(&mut self) {
        // No adding tasks while contract is paused
        assert_eq!(self.paused, false, "Task execution paused");

        // only registered agent signed, because micropayments will benefit long term
        let agent_opt = self.agents.get(&env::predecessor_account_id());
        if agent_opt.is_none() {
            env::panic(b"Agent not registered");
        }
        let mut agent = agent_opt.unwrap();

        // Get current slot based on block or timestamp
        let current_slot = self.get_slot_id(None);

        // get task based on current slot
        // priority goes to tasks that have fallen behind (using floor key)
        let (slot_opt, slot_ballpark) = if let Some(k) = self.slots.floor_key(&current_slot) {
            (self.slots.get(&k), k)
        } else {
            env::log(b"aloha ow my brain");
            (self.slots.get(&current_slot), current_slot)
        };

        let mut slot_data = slot_opt.expect("No tasks found in slot");

        // Get a single task hash, then retrieve task details
        let hash = slot_data.pop().expect("No tasks available");

        // After popping, ensure state is rewritten back
        if slot_data.is_empty() {
            // Clean up slot if no more data
            self.slots.remove(&slot_ballpark);
            log!("Slot {} cleaned");
        } else {
            self.slots.insert(&slot_ballpark, &slot_data);
        }

        let mut task = self.tasks.get(&hash).expect("No task found by hash");

        // Fee breakdown:
        // - Used Gas: Task Txn Fee Cost
        // - Agent Fee: Incentivize Execution SLA
        //
        // Task Fee Examples:
        // Total Fee = Gas Fee + Agent Fee
        // Total Balance = Task Deposit + Total Fee 
        //
        // NOTE: Gas cost includes the cross-contract call & internal logic of this contract.
        // Direct contract gas fee will be lower than task execution costs, however
        // we require the task owner to appropriately estimate gas for overpayment.
        // The gas overpayment will also accrue to the agent since there is no way to read
        // how much gas was actually used on callback.
        let call_fee_used = u128::from(task.gas) * self.gas_price;
        let call_total_fee = call_fee_used + self.agent_fee;
        let call_total_balance = task.deposit.0 + call_total_fee;

        // safety check and not burn too much gas.
        if call_total_balance > task.total_deposit.0 {
            log!("Not enough task balance to execute task, exiting");
            // Process task exit, if no future task can execute
            return self.exit_task(hash);
        }

        // Update agent storage
        // Increment agent reward & task count
        // Reward for agent MUST include the amount of gas used as a reimbursement
        agent.balance = U128::from(agent.balance.0 + call_total_fee);
        agent.total_tasks_executed = U128::from(agent.total_tasks_executed.0 + 1);
        self.agents.insert(&env::signer_account_id(), &agent);

        // Decrease task balance, Update task storage
        task.total_deposit = U128::from(task.total_deposit.0 - call_total_balance);
        self.tasks.insert(&hash, &task);

        // Call external contract with task variables
        let promise_first = env::promise_create(
            task.contract_id.clone(),
            &task.function_id.as_bytes(),
            task.arguments.0.as_slice(),
            task.deposit.0,
            task.gas,
        );

        // if out of balance or non-recurring, exit callback
        if !task.recurring || call_total_balance > task.total_deposit.0 {
            // Process task exit, if no future task can execute
            self.exit_task(hash);
            env::promise_return(promise_first);
        } else {
            // if recurring, callback for scheduling
            let promise_second = env::promise_then(
                promise_first,
                env::current_account_id(),
                b"callback_for_proxy_call",
                json!({
                    "task_hash": hash,
                    "current_slot": U128::from(current_slot)
                }).to_string().as_bytes(),
                0,
                GAS_FOR_CALLBACK,
            );
            env::promise_return(promise_second);
        }   
    }

    /// Logic executed on the completion of a proxy call
    /// Reschedule next task
    #[private]
    pub fn callback_for_proxy_call(&mut self, task_hash: Vec<u8>, current_slot: U128) {
        let task = self
            .tasks
            .get(&task_hash.clone())
            .expect("No task found by hash");

        // TODO: double check this can't get scheduled in current slot again
        let next_slot = self.get_slot_from_cadence(task.cadence.clone());
        log!("Scheduling Next Task {:?}", &next_slot);
        assert!(
            &current_slot.0 < &next_slot,
            "Cannot schedule task in the past"
        );

        // Get previous task hashes in slot, add as needed
        let mut slot_tasks = self.slots.get(&next_slot).unwrap_or_default();
        slot_tasks.push(task_hash.clone());
        self.slots.insert(&next_slot, &slot_tasks);
    }
}

// Internal methods
impl Contract {
    fn hash(&self, item: &Task) -> Vec<u8> {
        // Generate hash, needs to be from known values so we can reproduce the hash without storing
        let input = format!(
            "{:?}{:?}{:?}{:?}",
            item.contract_id, item.function_id, item.cadence, item.owner_id
        );
        env::sha256(input.as_bytes())
    }

    /// Returns the base amount required to execute 1 task
    /// NOTE: this is not the final used amount, just the user-specified amount total needed
    fn task_balance_uses(&self, task: &Task) -> u128 {
        task.deposit.0 + (u128::from(task.gas) * self.gas_price) + self.agent_fee
    }

    /// Check if a cadence string is valid by attempting to parse it
    fn validate_cadence(&self, cadence: &str) -> bool {
        let s = Schedule::from_str(&cadence);
        if s.is_ok() {
            true
        } else {
            false
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use near_sdk::json_types::ValidAccountId;
    use near_sdk::test_utils::{accounts, VMContextBuilder};
    use near_sdk::{testing_env, MockedBlockchain};

    use chrono::prelude::DateTime;
    use chrono::Utc;
    use chrono::*;
    use cron_schedule::Schedule;
    use std::str::FromStr;

    const BLOCK_START_BLOCK: u64 = 52_201_040;
    const BLOCK_START_TS: u64 = 1_624_151_503_447_000_000;

    pub fn get_sample_task() -> Task {
        Task {
            owner_id: String::from("bob"),
            contract_id: String::from("danny"),
            function_id: String::from("increment"),
            cadence: String::from("0 0 */1 * * *"),
            recurring: false,
            total_deposit: U128::from(1000000000020000000100),
            deposit: U128::from(100),
            gas: 200,
            arguments: Base64VecU8::from(vec![]),
        }
    }

    // from https://stackoverflow.com/a/50072164/711863
    pub fn human_readable_time(time_nano: u64) -> String {
        let timestamp = (time_nano / 1_000_000_000)
            .to_string()
            .parse::<i64>()
            .unwrap();
        let naive = NaiveDateTime::from_timestamp(timestamp, 0);
        let datetime: DateTime<Utc> = DateTime::from_utc(naive, Utc);
        let newdate = datetime.format("%Y-%m-%d %H:%M:%S");
        // Print the newly formatted date and time
        newdate.to_string()
    }

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

    #[test]
    fn test_task_create() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = Contract::new();
        testing_env!(context.is_view(true).build());
        assert!(contract.get_all_tasks(None).is_empty());
        testing_env!(context
            .is_view(false)
            .attached_deposit(1000000000020000000100)
            .build());
        let task_id = contract.create_task(
            accounts(3),
            "increment".to_string(),
            "0 0 */1 * * *".to_string(),
            Some(false),
            Some(U128::from(100)),
            Some(200),
            None,
        );

        testing_env!(context.is_view(true).build());
        assert_eq!(contract.get_all_tasks(None).len(), 1);

        let daily_task = get_sample_task();
        assert_eq!(contract.get_task(task_id), daily_task);
    }

    #[test]
    #[should_panic(expected = "Create task paused")]
    fn test_task_create_paused() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = Contract::new();
        testing_env!(context.is_view(false).build());
        contract.update_settings(None, None, Some(true), None, None, None);
        testing_env!(context
            .is_view(false)
            .attached_deposit(1000000000020000000100)
            .build());
        contract.create_task(
            accounts(3),
            "increment".to_string(),
            "0 0 */1 * * *".to_string(),
            Some(true),
            Some(U128::from(100)),
            Some(200),
            None,
        );
    }

    #[test]
    #[should_panic(expected = "Cadence string invalid")]
    fn test_task_create_bad_cadence() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = Contract::new();
        testing_env!(context
            .is_view(false)
            .attached_deposit(1000000000020000000100)
            .build());
        contract.create_task(
            accounts(3),
            "increment".to_string(),
            "raspberry_oat_milk".to_string(),
            Some(true),
            Some(U128::from(100)),
            Some(200),
            None,
        );
    }

    #[test]
    #[should_panic(
        expected = "Not enough task balance to execute job, need at least 1000000000020000100000"
    )]
    fn test_task_create_deposit_not_enuf() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = Contract::new();
        testing_env!(context.is_view(false).attached_deposit(0).build());
        contract.create_task(
            accounts(3),
            "increment".to_string(),
            "0 0 */1 * * *".to_string(),
            Some(false),
            Some(U128::from(100000)),
            Some(200),
            None,
        );
    }

    #[test]
    #[should_panic(
        expected = "Not enough task balance to execute job, need at least 2000000000040000200000"
    )]
    fn test_task_create_deposit_not_enuf_recurring() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = Contract::new();
        testing_env!(context.is_view(false).attached_deposit(0).build());
        contract.create_task(
            accounts(3),
            "increment".to_string(),
            "0 0 */1 * * *".to_string(),
            Some(true),
            Some(U128::from(100000)),
            Some(200),
            None,
        );
    }

    // NOTE: Useless when agent fee is higher than base gas
    // #[test]
    // #[should_panic(expected = "Gas minimum has not been met")]
    // fn test_task_create_gas_min() {
    //     let mut context = get_context(accounts(1));
    //     testing_env!(context.build());
    //     let mut contract = Contract::new();
    //     testing_env!(context.is_view(false).attached_deposit(206000000000000000).build());
    //     contract.create_task(
    //         accounts(3),
    //         "increment".to_string(),
    //         "0 0 */1 * * *".to_string(),
    //         Some(true),
    //         Some(U128::from(100000000000000000)),
    //         Some(0),
    //         None,
    //     );
    // }

    #[test]
    fn test_task_create_slot_schedule() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = Contract::new();

        // Move forward time and blocks to get more accurate bps
        testing_env!(context
            .is_view(false)
            .attached_deposit(1000000000020000000100)
            .block_timestamp(BLOCK_START_TS + (6 * NANO))
            .block_index(BLOCK_START_BLOCK + 6)
            .build());

        contract.create_task(
            accounts(3),
            "increment".to_string(),
            "*/10 * * * * *".to_string(),
            Some(false),
            Some(U128::from(0)),
            Some(200),
            None,
        );
        testing_env!(context.is_view(true).build());
        let slot = contract
            .slots
            .get(&52201080)
            .expect("Should have something here");
        assert_eq!(
            slot[0],
            [21, 209, 124, 71, 241, 6, 3, 102, 114, 186, 60, 89, 64, 69, 99, 43, 141, 4, 101, 196, 41, 133, 9, 73, 102, 127, 6, 197, 80, 247, 8, 116]
        );
    }

    #[test]
    fn test_task_get_only_active() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = Contract::new();

        // Move forward time and blocks to get more accurate bps
        testing_env!(context
            .is_view(false)
            .attached_deposit(1000000000020000000100)
            .block_timestamp(BLOCK_START_TS + (6 * NANO))
            .block_index(BLOCK_START_BLOCK + 6)
            .build());

        // create a some tasks
        contract.create_task(
            accounts(3),
            "increment".to_string(),
            "*/10 * * * * *".to_string(),
            Some(false),
            Some(U128::from(0)),
            Some(200),
            None,
        );
        contract.create_task(
            accounts(3),
            "decrement".to_string(),
            "*/10 * * * * *".to_string(),
            Some(false),
            Some(U128::from(0)),
            Some(200),
            None,
        );
        testing_env!(context
            .is_view(false)
            .attached_deposit(3000000000000300)
            .block_timestamp(BLOCK_START_TS + (12 * NANO))
            .block_index(BLOCK_START_BLOCK + 12)
            .build());
        testing_env!(context.is_view(true).build());
        println!(
            "contract.get_tasks(None) {:?}",
            contract.get_tasks(Some(1)).0.len()
        );
        assert_eq!(
            contract.get_tasks(Some(1)).0.len(),
            2,
            "Task amount diff than expected"
        );

        // change the tasks status
        // contract.proxy_call();
        // testing_env!(context.is_view(true).build());
        // assert_eq!(contract.get_tasks(Some(2)).0.len(), 0, "Task amount should be less");
    }

    // TODO: Finish
    // #[test]
    // fn test_task_proxy() {
    //     let mut context = get_context(accounts(1));
    //     testing_env!(context.build());
    //     let mut contract = Contract::new();
    //     testing_env!(context.is_view(false).attached_deposit(6000000000000).build());
    //     contract.create_task(
    //         accounts(3),
    //         "increment".to_string(),
    //         "*/10 * * * * *".to_string(),
    //         Some(false),
    //         None,
    //         None,
    //         None,
    //     );
    //     testing_env!(context.is_view(false).build());
    //     contract.register_agent(None);

    //     testing_env!(context.is_view(true).block_index(1260).build());
    //     assert!(contract.get_all_tasks(None).len() > 0);
    //     testing_env!(context.is_view(false).build());
    //     contract.proxy_call();
    //     assert!(contract.get_all_tasks(None).is_empty());
    // }

    #[test]
    // #[should_panic(expected = "Expected 1 promise result.")]
    #[should_panic(expected = "No task found by hash")]
    fn test_task_proxy_callback() {
        let context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = Contract::new();
        contract.callback_for_proxy_call(vec![0, 1, 2, 3], U128::from(123400));
    }

    #[test]
    #[should_panic(expected = "Agent not registered")]
    fn test_task_proxy_agent_not_registered() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = Contract::new();
        testing_env!(context
            .is_view(false)
            .attached_deposit(1000000000020000000100)
            .build());
        contract.create_task(
            accounts(3),
            "increment".to_string(),
            "0 0 */1 * * *".to_string(),
            Some(false),
            Some(U128::from(100)),
            Some(200),
            None,
        );
        testing_env!(context
            .is_view(false)
            .block_index(1260)
            .attached_deposit(3000000000000300)
            .prepaid_gas(300000000000)
            .build());
        contract.proxy_call();
    }

    #[test]
    #[should_panic(expected = "Task execution paused")]
    fn test_task_proxy_paused() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = Contract::new();
        testing_env!(context
            .is_view(false)
            .attached_deposit(1000000000020000000100)
            .build());
        contract.create_task(
            accounts(3),
            "increment".to_string(),
            "0 0 */1 * * *".to_string(),
            Some(false),
            Some(U128::from(100)),
            Some(200),
            None,
        );
        contract.update_settings(None, None, Some(true), None, None, None);
        testing_env!(context.is_view(false).block_index(1260).build());
        contract.proxy_call();
    }

    // TODO: No longer relevant because agent can use this opp to clean slots... need to check and validate this
    // #[test]
    // #[should_panic(expected = "No tasks found in slot")]
    // fn test_task_proxy_no_tasks() {
    //     let mut context = get_context(accounts(1));
    //     context.attached_deposit(2090000000000000000000);
    //     testing_env!(context.build());
    //     let mut contract = Contract::new();
    //     contract.register_agent(None);
    //     testing_env!(context.is_view(false).block_index(1260).build());
    //     contract.proxy_call();
    // }

    #[test]
    fn test_task_remove() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = Contract::new();
        testing_env!(context.is_view(true).build());
        assert!(contract.get_all_tasks(None).is_empty());
        testing_env!(context
            .is_view(false)
            .attached_deposit(ONE_NEAR * 100)
            .build());
        let task_hash = contract.create_task(
            accounts(3),
            "increment".to_string(),
            "0 0 */1 * * *".to_string(),
            Some(false),
            Some(U128::from(100)),
            Some(200),
            None,
        );

        testing_env!(context.is_view(true).build());
        assert_eq!(contract.get_all_tasks(None).len(), 1);

        testing_env!(context.is_view(false).build());
        contract.remove_task(task_hash);

        testing_env!(context.is_view(true).build());
        assert_eq!(contract.get_all_tasks(None).len(), 0);
    }

    #[test]
    #[should_panic(expected = "Only owner can remove their task.")]
    fn test_task_remove_not_owner() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = Contract::new();
        testing_env!(context.is_view(true).build());
        assert!(contract.get_all_tasks(None).is_empty());
        testing_env!(context
            .is_view(false)
            .attached_deposit(1000000000020000000100)
            .build());
        let task_hash = contract.create_task(
            accounts(3),
            "increment".to_string(),
            "0 0 */1 * * *".to_string(),
            Some(false),
            Some(U128::from(100)),
            Some(200),
            None,
        );

        testing_env!(context.is_view(true).build());
        assert_eq!(contract.get_all_tasks(None).len(), 1);

        testing_env!(context
            .is_view(false)
            .signer_account_id(accounts(4))
            .predecessor_account_id(accounts(4))
            .build());
        contract.remove_task(task_hash);
    }

    #[test]
    #[should_panic(expected = "No task found by hash")]
    fn test_task_remove_no_task() {
        let context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = Contract::new();
        contract.remove_task(Base64VecU8::from(vec![0, 1, 2, 3]));
    }

    #[test]
    fn test_get_slot_id_current_block() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let contract = Contract::new();
        testing_env!(context.is_view(true).build());
        let slot = contract.get_slot_id(None);

        assert_eq!(slot, 52201020);
    }

    #[test]
    fn test_get_slot_id_offset_block() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let contract = Contract::new();
        testing_env!(context.is_view(true).build());
        let slot = contract.get_slot_id(Some(1_000));

        assert_eq!(slot, 52201980);
    }

    #[test]
    fn test_get_slot_id_max_block() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let contract = Contract::new();
        testing_env!(context.is_view(true).build());
        let slot = contract.get_slot_id(Some(1_000_000_000_000));

        // ensure even if we pass in a HUGE number, it can only be scheduled UP to the max pre-defined block settings
        assert_eq!(slot, 1_000_052_200_980);
    }

    #[test]
    fn test_get_slot_id_change_granularity() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let mut contract = Contract::new();
        testing_env!(context.is_view(true).build());
        let slot = contract.get_slot_id(None);
        assert_eq!(slot, 52201020);

        testing_env!(context.is_view(false).build());
        contract.update_settings(None, Some(10), None, None, None, None);
        testing_env!(context.is_view(true).build());
        let slot = contract.get_slot_id(None);
        assert_eq!(slot, 52201040);

        testing_env!(context.is_view(false).build());
        contract.update_settings(None, Some(1), None, None, None, None);
        testing_env!(context.is_view(true).build());
        let slot = contract.get_slot_id(None);
        assert_eq!(slot, 52201040);
    }

    #[test]
    fn test_get_slot_from_cadence_ts_check() {
        // let start_ts: u64 = 1_624_151_500_000_000_000;
        let rem = BLOCK_START_TS.clone() % 1_000_000;
        let secs = ((BLOCK_START_TS.clone() - rem) / 1_000_000_000) + 1;
        let start_ts = Utc.timestamp(secs as i64, 0).naive_utc().timestamp_nanos() as u64;
        let context = get_context(accounts(1));
        testing_env!(context.build());
        let current_block_ts = env::block_timestamp();

        // Seconds
        let schedule1 = Schedule::from_str(&"*/5 * * * * *".to_string()).unwrap();
        let next_ts1 = schedule1.next_after(&current_block_ts).unwrap();
        println!("TS 1: {} {}", next_ts1, human_readable_time(next_ts1));
        let denom1 = 5 * NANO;
        let rem1 = start_ts.clone() % denom1;
        assert_eq!(next_ts1, (start_ts.clone() - rem1) + denom1);

        // Minutes
        let schedule2 = Schedule::from_str(&"* */5 * * * *".to_string()).unwrap();
        let next_ts2 = schedule2.next_after(&current_block_ts).unwrap();
        println!("TS 2: {} {}", next_ts2, human_readable_time(next_ts2));
        let denom2 = 5 * 60 * NANO;
        let rem2 = start_ts.clone() % denom2;
        assert_eq!(next_ts2, (start_ts.clone() - rem2) + denom2);

        // Hours
        let schedule3 = Schedule::from_str(&"* * */5 * * *".to_string()).unwrap();
        let next_ts3 = schedule3.next_after(&current_block_ts).unwrap();
        println!("TS 3: {} {}", next_ts3, human_readable_time(next_ts3));
        assert_eq!(next_ts3, 1624165200000000000);

        // Days
        let schedule4 = Schedule::from_str(&"* * * 10 * *".to_string()).unwrap();
        let next_ts4 = schedule4.next_after(&current_block_ts).unwrap();
        println!("TS 4: {} {}", next_ts4, human_readable_time(next_ts4));
        assert_eq!(next_ts4, 1625875200000000000);

        // Month
        let schedule5 = Schedule::from_str(&"* * * * 10 *".to_string()).unwrap();
        let next_ts5 = schedule5.next_after(&current_block_ts).unwrap();
        println!("TS 5: {} {}", next_ts5, human_readable_time(next_ts5));
        assert_eq!(next_ts5, 1633046400000000000);

        // Year
        let schedule6 = Schedule::from_str(&"* * * * * * 2025".to_string()).unwrap();
        let next_ts6 = schedule6.next_after(&current_block_ts).unwrap();
        println!("TS 6: {} {}", next_ts6, human_readable_time(next_ts6));
        assert_eq!(next_ts6, 1750381904000000000);
    }

    #[test]
    fn test_get_slot_from_cadence_match() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let contract = Contract::new();
        testing_env!(context
            .is_view(false)
            .block_index(BLOCK_START_BLOCK.clone() + 1)
            .block_timestamp(BLOCK_START_TS.clone() + 1_000_000_000)
            .build());
        testing_env!(context.is_view(true).build());
        let slot1 = contract.get_slot_from_cadence("*/5 * * * * *".to_string()); // Immediately next slot (since every 5 seconds)
        println!("SLOT 1 {}", slot1);
        assert_eq!(slot1, 52201080);
        let slot2 = contract.get_slot_from_cadence("* */5 * * * *".to_string()); // Every 5 mins
        println!("SLOT 2 {}", slot2);
        assert_eq!(slot2, 52201200);
        let slot3 = contract.get_slot_from_cadence("* * */5 * * *".to_string()); // Every 5th hour
        println!("SLOT 3 {}", slot3);
        assert_eq!(slot3, 52214700);
        let slot4 = contract.get_slot_from_cadence("* * * 10 * *".to_string()); // The 10th day of Month
        println!("SLOT 4 {}", slot4);
        assert_eq!(slot4, 53924700);
        let slot5 = contract.get_slot_from_cadence("* * * * 10 *".to_string()); // The 10th Month of the Year
        println!("SLOT 5 {}", slot5);
        assert_eq!(slot5, 61095900);
        let slot6 = contract.get_slot_from_cadence("* * * * * * 2025".to_string());
        println!("SLOT 6 {}", slot6);
        assert_eq!(slot6, 178431420);
    }

    #[test]
    fn test_hash_compute() {
        let context = get_context(accounts(3));
        testing_env!(context.build());
        let contract = Contract::new();
        let task = get_sample_task();
        let hash = contract.hash(&task);
        assert_eq!(
            hash,
            [32, 154, 253, 118, 34, 137, 134, 24, 119, 224, 187, 34, 173, 65, 86, 153, 220, 236, 185, 254, 202, 216, 153, 93, 113, 214, 29, 191, 129, 85, 146, 169],
            "Hash is not equivalent"
        )
    }
}
