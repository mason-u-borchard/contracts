use near_contract_standards::storage_management::StorageManagement;

use crate::*;

#[derive(BorshDeserialize, BorshSerialize, Debug, Serialize, Deserialize, PartialEq)]
#[serde(crate = "near_sdk::serde")]
pub struct Agent {
    pub payable_account_id: AccountId,
    pub balance: U128,
    pub total_tasks_executed: U128,
}

#[near_bindgen]
impl Contract {

    /// Add any account as an agent that will be able to execute tasks.
    /// Registering allows for rewards accruing with micro-payments which will accumulate to more long-term.
    ///
    /// Optional Parameters:
    /// "payable_account_id" - Allows a different account id to be specified, so a user can receive funds at a different account than the agent account.
    ///
    /// ```bash
    /// near call cron.testnet register_agent '{"payable_account_id": "YOU.testnet"}' --accountId YOUR_AGENT.testnet
    /// ```
    #[payable]
    pub fn register_agent(
        &mut self,
        payable_account_id: Option<ValidAccountId>,
    ) {
        assert_eq!(self.paused, false, "Register agent paused");

        let deposit: Balance = env::attached_deposit();
        let required_deposit: Balance =
            Balance::from(self.agent_storage_usage) * env::storage_byte_cost();

        assert!(
            deposit >= required_deposit,
            "Insufficient deposit. Please deposit {} yoctoⓃ to register an agent.",
            required_deposit.clone()
        );

        let account = env::predecessor_account_id();
        // check that account isn't already added
        if let Some(agent) = self.agents.get(&account) {
            let panic_msg = format!("Agent already exists: {:?}. Refunding the deposit.", agent);
            env::panic(panic_msg.as_bytes());
        };

        let payable_id = payable_account_id
            .map(|a| a.into())
            .unwrap_or_else(|| env::predecessor_account_id());

        let agent = Agent {
            payable_account_id: payable_id,
            balance: U128::from(required_deposit),
            total_tasks_executed: U128::from(0),
        };

        self.agents.insert(&account, &agent);

        // If the user deposited more than needed, refund them.
        let refund = deposit - required_deposit;
        if refund > 0 {
            Promise::new(env::predecessor_account_id()).transfer(refund);
        }
    }

    /// Update agent details, specifically the payable account id for an agent.
    ///
    /// ```bash
    /// near call cron.testnet update_agent '{"payable_account_id": "YOU.testnet"}' --accountId YOUR_AGENT.testnet
    /// ```
    #[payable]
    pub fn update_agent(&mut self, payable_account_id: Option<ValidAccountId>) {
        assert_eq!(self.paused, false, "Update agent paused");
        assert_one_yocto();

        let account = env::predecessor_account_id();

        // check that predecessor agent exists
        if let Some(mut agent) = self.agents.get(&account) {
            if payable_account_id.is_some() {
                agent.payable_account_id = payable_account_id.unwrap().into();
                self.agents.insert(&account, &agent);
            }
        } else {
            panic!("Agent must register");
        };
    }

    /// Removes the agent from the active set of agents.
    /// Withdraws all reward balances to the agent payable account id.
    /// Requires attaching 1 yoctoⓃ ensure it comes from a full-access key.
    ///
    /// ```bash
    /// near call cron.testnet unregister_agent --accountId YOUR_AGENT.testnet
    /// ```
    #[payable]
    pub fn unregister_agent(&mut self) {
        // This method name is quite explicit, so calling storage_unregister and setting the 'force' option to true.
        self.storage_unregister(Some(true));
    }

    /// Allows an agent to withdraw all rewards, paid to the specified payable account id.
    ///
    /// ```bash
    /// near call cron.testnet withdraw_task_balance --accountId YOUR_AGENT.testnet
    /// ```
    pub fn withdraw_task_balance(&mut self) -> Promise {
        let account = env::predecessor_account_id();
        let storage_fee = self.agent_storage_usage as u128 * env::storage_byte_cost();

        // check that signer agent exists
        if let Some(mut agent) = self.agents.get(&account) {
            let agent_balance = agent.balance.0;
            assert!(
                agent_balance > storage_fee,
                "No Agent balance beyond the storage balance"
            );
            let withdrawal_amount = agent_balance - storage_fee;
            agent.balance = U128::from(agent_balance - withdrawal_amount);
            self.agents.insert(&account, &agent);
            log!("Withdrawal of {} has been sent.", withdrawal_amount);
            Promise::new(agent.payable_account_id.to_string()).transfer(withdrawal_amount)
        } else {
            env::panic(b"No Agent")
        }
    }

    /// Gets the agent data stats
    ///
    /// ```bash
    /// near view cron.testnet get_agent '{"account": "YOUR_AGENT.testnet"}'
    /// ```
    pub fn get_agent(&self, account: AccountId) -> Option<Agent> {
        self.agents.get(&account)
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
    fn test_agent_register_check() {
        let mut context = get_context(accounts(1));
        testing_env!(context.build());
        let contract = Contract::new();
        testing_env!(context.is_view(true).build());
        assert!(contract.get_agent(accounts(1).to_string()).is_none());
    }

    #[test]
    fn test_agent_register_new() {
        let mut context = get_context(accounts(1));
        context.attached_deposit(2090000000000000000000);
        testing_env!(context.is_view(false).build());
        let mut contract = Contract::new();
        contract.register_agent(Some(accounts(1)));

        testing_env!(context.is_view(true).build());
        let _agent = contract.get_agent(accounts(1).to_string());
        assert_eq!(
            contract.get_agent(accounts(1).to_string()),
            Some(Agent {
                payable_account_id: accounts(1).to_string(),
                balance: U128::from(2090000000000000000000),
                total_tasks_executed: U128::from(0)
            })
        );
    }

    #[test]
    #[should_panic(expected = "Agent must register")]
    fn test_agent_update_check() {
        let mut context = get_context(accounts(1));
        context.attached_deposit(1);
        testing_env!(context.build());
        let mut contract = Contract::new();
        contract.update_agent(None);
        contract.update_agent(Some(accounts(2)));
    }

    #[test]
    fn test_agent_update() {
        let mut context = get_context(accounts(1));
        context.attached_deposit(2090000000000000000000);
        testing_env!(context.is_view(false).build());
        let mut contract = Contract::new();
        contract.register_agent(Some(accounts(1)));
        context.attached_deposit(1);
        testing_env!(context.build());
        contract.update_agent(Some(accounts(2)));

        testing_env!(context.is_view(true).build());
        let _agent = contract.get_agent(accounts(1).to_string());
        assert_eq!(
            contract.get_agent(accounts(1).to_string()),
            Some(Agent {
                payable_account_id: accounts(2).to_string(),
                balance: U128::from(2090000000000000000000),
                total_tasks_executed: U128::from(0)
            })
        );
    }

    #[test]
    fn test_agent_unregister_no_balance() {
        let mut context = get_context(accounts(1));
        context.attached_deposit(2090000000000000000000);
        testing_env!(context.is_view(false).build());
        let mut contract = Contract::new();
        contract.register_agent(Some(accounts(1)));
        context.attached_deposit(1);
        testing_env!(context.build());
        contract.unregister_agent();

        testing_env!(context.is_view(true).build());
        let _agent = contract.get_agent(accounts(1).to_string());
        assert_eq!(contract.get_agent(accounts(1).to_string()), None);
    }

    #[test]
    #[should_panic(expected = "No Agent")]
    fn test_agent_withdraw_check() {
        let context = get_context(accounts(3));
        testing_env!(context.build());
        let mut contract = Contract::new();
        contract.withdraw_task_balance();
    }

    #[test]
    fn agent_storage_check() {
        let context = get_context(accounts(1));
        testing_env!(context.build());
        let contract = Contract::new();
        assert_eq!(
            209, contract.agent_storage_usage,
            "Expected different storage usage for the agent."
        );
    }
}
