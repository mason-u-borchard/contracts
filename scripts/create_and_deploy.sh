#!/bin/bash
# This file is used for starting a fresh set of all contracts & configs
set -e

if [ -d "res" ]; then
  echo ""
else
  mkdir res
fi

cd "`dirname $0`"

if [ -z "$KEEP_NAMES" ]; then
  export RUSTFLAGS='-C link-arg=-s'
else
  export RUSTFLAGS=''
fi

# build the things
cargo build --all --target wasm32-unknown-unknown --release
cp ../target/wasm32-unknown-unknown/release/*.wasm ./res/

# Uncomment the desired network
export NEAR_ENV=testnet
# export NEAR_ENV=mainnet
# export NEAR_ENV=guildnet
# export NEAR_ENV=betanet

export FACTORY=testnet
# export FACTORY=near
# export FACTORY=registrar

if [ -z ${NEAR_ACCT+x} ]; then
  export NEAR_ACCT=you.testnet
else
  export NEAR_ACCT=$NEAR_ACCT
fi

export CRON_ACCOUNT_ID=cron.$NEAR_ACCT
export COUNTER_ACCOUNT_ID=counter.$NEAR_ACCT
export AGENT_ACCOUNT_ID=agent.$NEAR_ACCT
export USER_ACCOUNT_ID=user.$NEAR_ACCT
export CRUD_ACCOUNT_ID=crud.$NEAR_ACCT
export DAO_ACCOUNT_ID=dao.sputnikv2.testnet

# create all accounts
near create-account $CRON_ACCOUNT_ID --masterAccount $NEAR_ACCT
near create-account $COUNTER_ACCOUNT_ID --masterAccount $NEAR_ACCT
near create-account $AGENT_ACCOUNT_ID --masterAccount $NEAR_ACCT
near create-account $USER_ACCOUNT_ID --masterAccount $NEAR_ACCT
near create-account $CRUD_ACCOUNT_ID --masterAccount $NEAR_ACCT

# Deploy all the contracts to their rightful places
near deploy --wasmFile ./res/manager.wasm --accountId cron.$NEAR_ACCT --initFunction new --initArgs '{}'
near deploy --wasmFile ./res/rust_counter_tutorial.wasm --accountId counter.$NEAR_ACCT
near deploy --wasmFile ./res/cross_contract.wasm --accountId crud.$NEAR_ACCT --initFunction new --initArgs '{"cron": "cron.'$NEAR_ACCT'"}'

echo "Setup Complete"