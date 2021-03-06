#![feature(array_methods)]
#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(not(feature = "std"), feature(alloc_error_handler))]
#![cfg_attr(feature = "log", feature(panic_info_message))]

use aurora_engine_types::parameters::PromiseCreateArgs;

#[cfg(not(feature = "std"))]
extern crate alloc;
#[cfg(not(feature = "std"))]
extern crate core;

mod map;
#[cfg(feature = "meta-call")]
pub mod meta_parsing;
pub mod parameters;
pub mod proof;
pub mod transaction;

pub mod admin_controlled;
#[cfg_attr(feature = "contract", allow(dead_code))]
pub mod connector;
pub mod deposit_event;
pub mod engine;
pub mod fungible_token;
pub mod json;
pub mod log_entry;
mod prelude;

#[cfg(target_arch = "wasm32")]
#[global_allocator]
static ALLOC: wee_alloc::WeeAlloc = wee_alloc::WeeAlloc::INIT;

#[cfg(target_arch = "wasm32")]
#[panic_handler]
#[cfg_attr(not(feature = "log"), allow(unused_variables))]
#[no_mangle]
pub unsafe fn on_panic(info: &::core::panic::PanicInfo) -> ! {
    #[cfg(feature = "log")]
    {
        use prelude::ToString;

        if let Some(msg) = info.message() {
            let msg = if let Some(log) = info.location() {
                prelude::format!("{} [{}]", msg, log)
            } else {
                msg.to_string()
            };
            prelude::sdk::panic_utf8(msg.as_bytes());
        } else if let Some(log) = info.location() {
            prelude::sdk::panic_utf8(log.to_string().as_bytes());
        }
    }

    ::core::arch::wasm32::unreachable();
}

#[cfg(target_arch = "wasm32")]
#[alloc_error_handler]
#[no_mangle]
pub unsafe fn on_alloc_error(_: core::alloc::Layout) -> ! {
    ::core::arch::wasm32::unreachable();
}

#[cfg(feature = "contract")]
mod contract {
    use borsh::{BorshDeserialize, BorshSerialize};

    use crate::connector::EthConnectorContract;
    use crate::engine::{self, current_address, Engine, EngineState, GasPaymentError};
    use crate::fungible_token::FungibleTokenMetadata;
    #[cfg(feature = "evm_bully")]
    use crate::parameters::{BeginBlockArgs, BeginChainArgs};
    use crate::parameters::{
        CallArgs, DeployErc20TokenArgs, GetErc20FromNep141CallArgs, GetStorageAtArgs, InitCallArgs,
        IsUsedProofCallArgs, NEP141FtOnTransferArgs, NewCallArgs, PauseEthConnectorCallArgs,
        ResolveTransferCallArgs, SetContractDataCallArgs, StorageDepositCallArgs,
        StorageWithdrawCallArgs, SubmitResult, TransactionStatus, TransferCallCallArgs,
        ViewCallArgs,
    };
    use aurora_engine_sdk::env::Env;
    use aurora_engine_sdk::io::{StorageIntermediate, IO};
    use aurora_engine_sdk::near_runtime::Runtime;
    use aurora_engine_sdk::promise::PromiseHandler;
    use aurora_engine_types::account_id::AccountId;

    use crate::json::parse_json;
    use crate::prelude::parameters::RefundCallArgs;
    use crate::prelude::sdk::types::{
        near_account_to_evm_address, SdkExpect, SdkProcess, SdkUnwrap,
    };
    use crate::prelude::storage::{bytes_to_key, KeyPrefix};
    use crate::prelude::types::{u256_to_arr, ERR_FAILED_PARSE};
    use crate::prelude::{
        sdk, vec, Address, PromiseResult, ToString, TryFrom, TryInto, Vec, Wei,
        ERC20_MINT_SELECTOR, H160, H256, U256,
    };
    use crate::transaction::{EthTransactionKind, NormalizedEthTransaction};

    const CODE_KEY: &[u8; 4] = b"CODE";
    const CODE_STAGE_KEY: &[u8; 10] = b"CODE_STAGE";
    const GAS_OVERFLOW: &str = "ERR_GAS_OVERFLOW";
    const PROMISE_COUNT_ERR: &str = "ERR_PROMISE_COUNT";

    ///
    /// ADMINISTRATIVE METHODS
    ///

    /// Sets the configuration for the Engine.
    /// Should be called on deployment.
    #[no_mangle]
    pub extern "C" fn new() {
        let mut io = Runtime;
        if let Ok(state) = engine::get_state(&io) {
            require_owner_only(&state, &io.predecessor_account_id());
        }

        let args: NewCallArgs = io.read_input_borsh().sdk_unwrap();
        engine::set_state(&mut io, args.into());
    }

    /// Get version of the contract.
    #[no_mangle]
    pub extern "C" fn get_version() {
        let mut io = Runtime;
        let version = match option_env!("NEAR_EVM_VERSION") {
            Some(v) => v.as_bytes(),
            None => include_bytes!("../../VERSION"),
        };
        io.return_output(version)
    }

    /// Get owner account id for this contract.
    #[no_mangle]
    pub extern "C" fn get_owner() {
        let mut io = Runtime;
        let state = engine::get_state(&io).sdk_unwrap();
        io.return_output(state.owner_id.as_bytes());
    }

    /// Get bridge prover id for this contract.
    #[no_mangle]
    pub extern "C" fn get_bridge_prover() {
        let mut io = Runtime;
        let state = engine::get_state(&io).sdk_unwrap();
        io.return_output(state.bridge_prover_id.as_bytes());
    }

    /// Get chain id for this contract.
    #[no_mangle]
    pub extern "C" fn get_chain_id() {
        let mut io = Runtime;
        io.return_output(&engine::get_state(&io).sdk_unwrap().chain_id)
    }

    #[no_mangle]
    pub extern "C" fn get_upgrade_index() {
        let mut io = Runtime;
        let state = engine::get_state(&io).sdk_unwrap();
        let index = internal_get_upgrade_index();
        io.return_output(&(index + state.upgrade_delay_blocks).to_le_bytes())
    }

    /// Stage new code for deployment.
    #[no_mangle]
    pub extern "C" fn stage_upgrade() {
        let mut io = Runtime;
        let state = engine::get_state(&io).sdk_unwrap();
        let block_height = io.block_height();
        require_owner_only(&state, &io.predecessor_account_id());
        io.read_input_and_store(&bytes_to_key(KeyPrefix::Config, CODE_KEY));
        io.write_storage(
            &bytes_to_key(KeyPrefix::Config, CODE_STAGE_KEY),
            &block_height.to_le_bytes(),
        );
    }

    /// Deploy staged upgrade.
    #[no_mangle]
    pub extern "C" fn deploy_upgrade() {
        let io = Runtime;
        let state = engine::get_state(&io).sdk_unwrap();
        let index = internal_get_upgrade_index();
        if io.block_height() <= index + state.upgrade_delay_blocks {
            sdk::panic_utf8(b"ERR_NOT_ALLOWED:TOO_EARLY");
        }
        Runtime::self_deploy(&bytes_to_key(KeyPrefix::Config, CODE_KEY));
    }

    /// Called as part of the upgrade process (see `engine-sdk::self_deploy`). This function is meant
    /// to make any necessary changes to the state such that it aligns with the newly deployed
    /// code.
    #[no_mangle]
    pub extern "C" fn state_migration() {
        // TODO: currently we don't have migrations
    }

    ///
    /// MUTATIVE METHODS
    ///

    /// Deploy code into the EVM.
    #[no_mangle]
    pub extern "C" fn deploy_code() {
        let io = Runtime;
        let input = io.read_input().to_vec();
        let current_account_id = io.current_account_id();
        let mut engine = Engine::new(
            predecessor_address(&io.predecessor_account_id()),
            current_account_id,
            io,
            &io,
        )
        .sdk_unwrap();
        Engine::deploy_code_with_input(&mut engine, input, &mut Runtime)
            .map(|res| res.try_to_vec().sdk_expect("ERR_SERIALIZE"))
            .sdk_process();
        // TODO: charge for storage
    }

    /// Call method on the EVM contract.
    #[no_mangle]
    pub extern "C" fn call() {
        let io = Runtime;
        let bytes = io.read_input().to_vec();
        let args = CallArgs::deserialize(&bytes).sdk_expect("ERR_BORSH_DESERIALIZE");
        let current_account_id = io.current_account_id();
        let mut engine = Engine::new(
            predecessor_address(&io.predecessor_account_id()),
            current_account_id,
            io,
            &io,
        )
        .sdk_unwrap();
        Engine::call_with_args(&mut engine, args, &mut Runtime)
            .map(|res| res.try_to_vec().sdk_expect("ERR_SERIALIZE"))
            .sdk_process();
        // TODO: charge for storage
    }

    /// Process signed Ethereum transaction.
    /// Must match CHAIN_ID to make sure it's signed for given chain vs replayed from another chain.
    #[no_mangle]
    pub extern "C" fn submit() {
        let mut io = Runtime;
        let input = io.read_input().to_vec();

        let transaction: NormalizedEthTransaction = EthTransactionKind::try_from(input.as_slice())
            .sdk_unwrap()
            .into();

        let state = engine::get_state(&io).sdk_unwrap();

        // Validate the chain ID, if provided inside the signature:
        if let Some(chain_id) = transaction.chain_id {
            if U256::from(chain_id) != U256::from(state.chain_id) {
                sdk::panic_utf8(b"ERR_INVALID_CHAIN_ID");
            }
        }

        // Retrieve the signer of the transaction:
        let sender = transaction
            .address
            .sdk_expect("ERR_INVALID_ECDSA_SIGNATURE");

        #[cfg(feature = "log")]
        sdk::log(crate::prelude::format!("signer_address {:?}", sender).as_str());

        engine::check_nonce(&io, &sender, &transaction.nonce).sdk_unwrap();

        // Check intrinsic gas is covered by transaction gas limit
        match transaction.intrinsic_gas(crate::engine::CONFIG) {
            None => sdk::panic_utf8(GAS_OVERFLOW.as_bytes()),
            Some(intrinsic_gas) => {
                if transaction.gas_limit < intrinsic_gas.into() {
                    sdk::panic_utf8(b"ERR_INTRINSIC_GAS")
                }
            }
        }

        if transaction.max_priority_fee_per_gas > transaction.max_fee_per_gas {
            sdk::panic_utf8(b"ERR_MAX_PRIORITY_FEE_GREATER")
        }

        // Figure out what kind of a transaction this is, and execute it:
        let current_account_id = io.current_account_id();
        let mut engine = Engine::new_with_state(state, sender, current_account_id, io, &io);
        let prepaid_amount = match engine.charge_gas(&sender, &transaction) {
            Ok(gas_result) => gas_result,
            Err(GasPaymentError::OutOfFund) => {
                engine::increment_nonce(&mut io, &sender);
                let result = SubmitResult::new(TransactionStatus::OutOfFund, 0, vec![]);
                io.return_output(&result.try_to_vec().unwrap());
                return;
            }
            Err(err) => sdk::panic_utf8(err.as_ref()),
        };
        let gas_limit: u64 = transaction.gas_limit.try_into().sdk_expect(GAS_OVERFLOW);
        let access_list = transaction
            .access_list
            .into_iter()
            .map(|a| (a.address, a.storage_keys))
            .collect();
        let result = if let Some(receiver) = transaction.to {
            Engine::call(
                &mut engine,
                sender,
                receiver,
                transaction.value,
                transaction.data,
                gas_limit,
                access_list,
                &mut Runtime,
            )
            // TODO: charge for storage
        } else {
            // Execute a contract deployment:
            Engine::deploy_code(
                &mut engine,
                sender,
                transaction.value,
                transaction.data,
                gas_limit,
                access_list,
                &mut Runtime,
            )
            // TODO: charge for storage
        };

        // Give refund
        let relayer = predecessor_address(&io.predecessor_account_id());
        let gas_used = match &result {
            Ok(submit_result) => submit_result.gas_used,
            Err(engine_err) => engine_err.gas_used,
        };
        engine::refund_unused_gas(&mut io, &sender, gas_used, prepaid_amount, &relayer)
            .sdk_unwrap();

        // return result to user
        result
            .map(|res| res.try_to_vec().sdk_expect("ERR_SERIALIZE"))
            .sdk_process();
    }

    #[cfg(feature = "meta-call")]
    #[no_mangle]
    pub extern "C" fn meta_call() {
        let io = Runtime;
        let input = io.read_input().to_vec();
        let state = engine::get_state(&io).sdk_unwrap();
        let domain_separator = crate::meta_parsing::near_erc712_domain(U256::from(state.chain_id));
        let meta_call_args = crate::meta_parsing::parse_meta_call(
            &domain_separator,
            io.current_account_id().as_bytes(),
            input,
        )
        .sdk_expect("ERR_META_TX_PARSE");

        engine::check_nonce(&io, &meta_call_args.sender, &meta_call_args.nonce).sdk_unwrap();

        let current_account_id = io.current_account_id();
        let mut engine =
            Engine::new_with_state(state, meta_call_args.sender, current_account_id, io, &io);
        let result = engine.call(
            meta_call_args.sender,
            meta_call_args.contract_address,
            meta_call_args.value,
            meta_call_args.input,
            u64::MAX, // TODO: is there a gas limit with meta calls?
            crate::prelude::Vec::new(),
            &mut Runtime,
        );
        result
            .map(|res| res.try_to_vec().sdk_expect("ERR_SERIALIZE"))
            .sdk_process();
    }

    #[no_mangle]
    pub extern "C" fn register_relayer() {
        let io = Runtime;
        let relayer_address = io.read_input_arr20().sdk_unwrap();

        let current_account_id = io.current_account_id();
        let predecessor_account_id = io.predecessor_account_id();
        let mut engine = Engine::new(
            predecessor_address(&predecessor_account_id),
            current_account_id,
            io,
            &io,
        )
        .sdk_unwrap();
        engine.register_relayer(predecessor_account_id.as_bytes(), Address(relayer_address));
    }

    /// Allow receiving NEP141 tokens to the EVM contract.
    ///
    /// This function returns the amount of tokens to return to the sender.
    /// Either all tokens are transferred tokens are returned in case of an
    /// error, or no token is returned if tx was successful.
    #[no_mangle]
    pub extern "C" fn ft_on_transfer() {
        let io = Runtime;
        let current_account_id = io.current_account_id();
        let predecessor_account_id = io.predecessor_account_id();
        let mut engine = Engine::new(
            predecessor_address(&predecessor_account_id),
            current_account_id.clone(),
            io,
            &io,
        )
        .sdk_unwrap();

        let args: NEP141FtOnTransferArgs = parse_json(io.read_input().to_vec().as_slice())
            .sdk_unwrap()
            .try_into()
            .sdk_unwrap();

        if predecessor_account_id == current_account_id {
            EthConnectorContract::get_instance(io).ft_on_transfer(&engine, &args);
        } else {
            let signer_account_id = io.signer_account_id();
            engine.receive_erc20_tokens(
                &predecessor_account_id,
                &signer_account_id,
                &args,
                &current_account_id,
                &mut Runtime,
            );
        }
    }

    /// Deploy ERC20 token mapped to a NEP141
    #[no_mangle]
    pub extern "C" fn deploy_erc20_token() {
        let mut io = Runtime;
        // Id of the NEP141 token in Near
        let args: DeployErc20TokenArgs = io.read_input_borsh().sdk_unwrap();

        let current_account_id = io.current_account_id();
        let erc20_admin_address = current_address(&current_account_id);
        let mut engine = Engine::new(
            predecessor_address(&io.predecessor_account_id()),
            current_account_id,
            io,
            &io,
        )
        .sdk_unwrap();

        #[cfg(feature = "error_refund")]
        let erc20_contract = include_bytes!("../../etc/eth-contracts/res/EvmErc20V2.bin");
        #[cfg(not(feature = "error_refund"))]
        let erc20_contract = include_bytes!("../../etc/eth-contracts/res/EvmErc20.bin");

        let deploy_args = ethabi::encode(&[
            ethabi::Token::String("Empty".to_string()),
            ethabi::Token::String("EMPTY".to_string()),
            ethabi::Token::Uint(ethabi::Uint::from(0)),
            ethabi::Token::Address(erc20_admin_address),
        ]);

        let address = match Engine::deploy_code_with_input(
            &mut engine,
            (&[erc20_contract, deploy_args.as_slice()].concat()).to_vec(),
            &mut Runtime,
        ) {
            Ok(result) => match result.status {
                TransactionStatus::Succeed(ret) => H160(ret.as_slice().try_into().unwrap()),
                other => sdk::panic_utf8(other.as_ref()),
            },
            Err(e) => sdk::panic_utf8(e.as_ref()),
        };

        sdk::log!(crate::prelude::format!("Deployed ERC-20 in Aurora at: {:#?}", address).as_str());
        engine.register_token(address, args.nep141).sdk_unwrap();
        io.return_output(&address.as_bytes().try_to_vec().sdk_expect("ERR_SERIALIZE"));

        // TODO: charge for storage
    }

    /// Callback invoked by exit to NEAR precompile to handle potential
    /// errors in the exit call.
    #[no_mangle]
    pub extern "C" fn refund_on_error() {
        let io = Runtime;
        io.assert_private_call().sdk_unwrap();

        // This function should only be called as the callback of
        // exactly one promise.
        if io.promise_results_count() != 1 {
            sdk::panic_utf8(PROMISE_COUNT_ERR.as_bytes());
        }

        if let Some(PromiseResult::Successful(_)) = io.promise_result(0) {
            // Promise succeeded -- nothing to do
        } else {
            // Exit call failed; need to refund tokens

            let current_account_id = io.current_account_id();
            let args: RefundCallArgs = io.read_input_borsh().sdk_unwrap();
            let refund_result = match args.erc20_address {
                // ERC-20 exit; re-mint burned tokens
                Some(erc20_address) => {
                    let erc20_admin_address = current_address(&current_account_id);
                    let mut engine =
                        Engine::new(erc20_admin_address, current_account_id, io, &io).sdk_unwrap();
                    let erc20_address = Address(erc20_address);
                    let refund_address = Address(args.recipient_address);
                    let amount = U256::from_big_endian(&args.amount);

                    let selector = ERC20_MINT_SELECTOR;
                    let mint_args = ethabi::encode(&[
                        ethabi::Token::Address(refund_address),
                        ethabi::Token::Uint(amount),
                    ]);

                    engine
                        .call(
                            erc20_admin_address,
                            erc20_address,
                            Wei::zero(),
                            [selector, mint_args.as_slice()].concat(),
                            u64::MAX,
                            Vec::new(),
                            &mut Runtime,
                        )
                        .sdk_unwrap()
                }
                // ETH exit; transfer ETH back from precompile address
                None => {
                    let exit_address = aurora_engine_precompiles::native::ExitToNear::ADDRESS;
                    let mut engine =
                        Engine::new(exit_address, current_account_id, io, &io).sdk_unwrap();
                    let refund_address = Address(args.recipient_address);
                    let amount = Wei::new(U256::from_big_endian(&args.amount));
                    engine
                        .call(
                            exit_address,
                            refund_address,
                            amount,
                            Vec::new(),
                            u64::MAX,
                            vec![(exit_address, Vec::new()), (refund_address, Vec::new())],
                            &mut Runtime,
                        )
                        .sdk_unwrap()
                }
            };

            if !refund_result.status.is_ok() {
                sdk::panic_utf8(b"ERR_REFUND_FAILURE");
            }
        }
    }
    ///
    /// NONMUTATIVE METHODS
    ///
    #[no_mangle]
    pub extern "C" fn view() {
        let mut io = Runtime;
        let args: ViewCallArgs = io.read_input_borsh().sdk_unwrap();
        let current_account_id = io.current_account_id();
        let engine = Engine::new(
            Address::from_slice(&args.sender),
            current_account_id,
            io,
            &io,
        )
        .sdk_unwrap();
        let result = Engine::view_with_args(&engine, args).sdk_unwrap();
        io.return_output(&result.try_to_vec().sdk_expect("ERR_SERIALIZE"));
    }

    #[no_mangle]
    pub extern "C" fn get_block_hash() {
        let mut io = Runtime;
        let block_height = io.read_input_borsh().sdk_unwrap();
        let account_id = io.current_account_id();
        let chain_id = engine::get_state(&io)
            .map(|state| state.chain_id)
            .sdk_unwrap();
        let block_hash =
            crate::engine::compute_block_hash(chain_id, block_height, account_id.as_bytes());
        io.return_output(block_hash.as_bytes())
    }

    #[no_mangle]
    pub extern "C" fn get_code() {
        let mut io = Runtime;
        let address = io.read_input_arr20().sdk_unwrap();
        let code = engine::get_code(&io, &Address(address));
        io.return_output(&code)
    }

    #[no_mangle]
    pub extern "C" fn get_balance() {
        let mut io = Runtime;
        let address = io.read_input_arr20().sdk_unwrap();
        let balance = engine::get_balance(&io, &Address(address));
        io.return_output(&balance.to_bytes())
    }

    #[no_mangle]
    pub extern "C" fn get_nonce() {
        let mut io = Runtime;
        let address = io.read_input_arr20().sdk_unwrap();
        let nonce = engine::get_nonce(&io, &Address(address));
        io.return_output(&u256_to_arr(&nonce))
    }

    #[no_mangle]
    pub extern "C" fn get_storage_at() {
        let mut io = Runtime;
        let args: GetStorageAtArgs = io.read_input_borsh().sdk_unwrap();
        let address = Address(args.address);
        let generation = engine::get_generation(&io, &address);
        let value = engine::get_storage(&io, &Address(args.address), &H256(args.key), generation);
        io.return_output(&value.0)
    }

    ///
    /// BENCHMARKING METHODS
    ///
    #[cfg(feature = "evm_bully")]
    #[no_mangle]
    pub extern "C" fn begin_chain() {
        let mut io = Runtime;
        let mut state = engine::get_state(&io).sdk_unwrap();
        require_owner_only(&state, &io.predecessor_account_id());
        let args: BeginChainArgs = io.read_input_borsh().sdk_unwrap();
        state.chain_id = args.chain_id;
        engine::set_state(&mut io, state);
        // set genesis block balances
        for account_balance in args.genesis_alloc {
            engine::set_balance(
                &mut io,
                &Address(account_balance.address),
                &crate::prelude::types::Wei::new(U256::from(account_balance.balance)),
            )
        }
        // return new chain ID
        io.return_output(&engine::get_state(&io).sdk_unwrap().chain_id)
    }

    #[cfg(feature = "evm_bully")]
    #[no_mangle]
    pub extern "C" fn begin_block() {
        let io = Runtime;
        let state = engine::get_state(&io).sdk_unwrap();
        require_owner_only(&state, &io.predecessor_account_id());
        let _args: BeginBlockArgs = io.read_input_borsh().sdk_unwrap();
        // TODO: https://github.com/aurora-is-near/aurora-engine/issues/2
    }

    #[no_mangle]
    pub extern "C" fn new_eth_connector() {
        let io = Runtime;
        // Only the owner can initialize the EthConnector
        io.assert_private_call().sdk_unwrap();

        let args: InitCallArgs = io.read_input_borsh().sdk_unwrap();
        let owner_id = io.current_account_id();

        EthConnectorContract::init_contract(io, owner_id, args);
    }

    #[no_mangle]
    pub extern "C" fn set_eth_connector_contract_data() {
        let mut io = Runtime;
        // Only the owner can set the EthConnector contract data
        io.assert_private_call().sdk_unwrap();

        let args: SetContractDataCallArgs = io.read_input_borsh().sdk_unwrap();
        EthConnectorContract::set_contract_data(&mut io, args);
    }

    #[no_mangle]
    pub extern "C" fn withdraw() {
        let mut io = Runtime;
        io.assert_one_yocto().sdk_unwrap();
        let args = io.read_input_borsh().sdk_unwrap();
        let current_account_id = io.current_account_id();
        let predecessor_account_id = io.predecessor_account_id();
        let result = EthConnectorContract::get_instance(io).withdraw_eth_from_near(
            &current_account_id,
            &predecessor_account_id,
            args,
        );
        let result_bytes = result.try_to_vec().sdk_expect("ERR_SERIALIZE");
        io.return_output(&result_bytes);
    }

    #[no_mangle]
    pub extern "C" fn deposit() {
        let mut io = Runtime;
        let raw_proof = io.read_input().to_vec();
        let current_account_id = io.current_account_id();
        let predecessor_account_id = io.predecessor_account_id();
        let promise_args = EthConnectorContract::get_instance(io).deposit(
            raw_proof,
            current_account_id,
            predecessor_account_id,
        );
        let promise_id = io.promise_crate_with_callback(&promise_args);
        io.promise_return(promise_id);
    }

    #[no_mangle]
    pub extern "C" fn finish_deposit() {
        let mut io = Runtime;
        io.assert_private_call().sdk_unwrap();

        // Check result from proof verification call
        if io.promise_results_count() != 1 {
            sdk::panic_utf8(PROMISE_COUNT_ERR.as_bytes());
        }
        let promise_result = match io.promise_result(0) {
            Some(PromiseResult::Successful(bytes)) => {
                bool::try_from_slice(&bytes).sdk_expect("ERR_PROMISE_ENCODING")
            }
            _ => sdk::panic_utf8(b"ERR_PROMISE_FAILED"),
        };
        if !promise_result {
            sdk::panic_utf8(b"ERR_VERIFY_PROOF");
        }

        let data = io.read_input_borsh().sdk_unwrap();
        let current_account_id = io.current_account_id();
        let predecessor_account_id = io.predecessor_account_id();
        let maybe_promise_args = EthConnectorContract::get_instance(io).finish_deposit(
            predecessor_account_id,
            current_account_id,
            data,
        );

        if let Some(promise_args) = maybe_promise_args {
            let promise_id = io.promise_crate_with_callback(&promise_args);
            io.promise_return(promise_id);
        }
    }

    #[no_mangle]
    pub extern "C" fn is_used_proof() {
        let mut io = Runtime;
        let args: IsUsedProofCallArgs = io.read_input_borsh().sdk_unwrap();

        let is_used_proof = EthConnectorContract::get_instance(io).is_used_proof(args.proof);
        let res = is_used_proof.try_to_vec().unwrap();
        io.return_output(&res[..]);
    }

    #[no_mangle]
    pub extern "C" fn ft_total_supply() {
        let io = Runtime;
        EthConnectorContract::get_instance(io).ft_total_eth_supply_on_near();
    }

    #[no_mangle]
    pub extern "C" fn ft_total_eth_supply_on_near() {
        let io = Runtime;
        EthConnectorContract::get_instance(io).ft_total_eth_supply_on_near();
    }

    #[no_mangle]
    pub extern "C" fn ft_total_eth_supply_on_aurora() {
        let io = Runtime;
        EthConnectorContract::get_instance(io).ft_total_eth_supply_on_aurora();
    }

    #[no_mangle]
    pub extern "C" fn ft_balance_of() {
        let io = Runtime;
        EthConnectorContract::get_instance(io).ft_balance_of();
    }

    #[no_mangle]
    pub extern "C" fn ft_balance_of_eth() {
        let io = Runtime;
        EthConnectorContract::get_instance(io).ft_balance_of_eth_on_aurora();
    }

    #[no_mangle]
    pub extern "C" fn ft_transfer() {
        let io = Runtime;
        io.assert_one_yocto().sdk_unwrap();
        let predecessor_account_id = io.predecessor_account_id();
        EthConnectorContract::get_instance(io).ft_transfer(&predecessor_account_id);
    }

    #[no_mangle]
    pub extern "C" fn ft_resolve_transfer() {
        let io = Runtime;

        io.assert_private_call().sdk_unwrap();
        if io.promise_results_count() != 1 {
            sdk::panic_utf8(PROMISE_COUNT_ERR.as_bytes());
        }

        let args: ResolveTransferCallArgs = io.read_input().to_value().sdk_unwrap();
        let promise_result = io.promise_result(0).sdk_unwrap();

        EthConnectorContract::get_instance(io).ft_resolve_transfer(args, promise_result);
    }

    #[no_mangle]
    pub extern "C" fn ft_transfer_call() {
        use sdk::types::ExpectUtf8;
        let mut io = Runtime;
        // Check is payable
        io.assert_one_yocto().sdk_unwrap();

        let args = TransferCallCallArgs::from(
            parse_json(&io.read_input().to_vec()).expect_utf8(ERR_FAILED_PARSE.as_bytes()),
        );
        let current_account_id = io.current_account_id();
        let predecessor_account_id = io.predecessor_account_id();
        let promise_args = EthConnectorContract::get_instance(io).ft_transfer_call(
            predecessor_account_id,
            current_account_id,
            args,
        );
        let promise_id = io.promise_crate_with_callback(&promise_args);
        io.promise_return(promise_id);
    }

    #[no_mangle]
    pub extern "C" fn storage_deposit() {
        let mut io = Runtime;
        let args = StorageDepositCallArgs::from(parse_json(&io.read_input().to_vec()).sdk_unwrap());
        let predecessor_account_id = io.predecessor_account_id();
        let amount = io.attached_deposit();
        let maybe_promise = EthConnectorContract::get_instance(io).storage_deposit(
            predecessor_account_id,
            amount,
            args,
        );
        if let Some(promise) = maybe_promise {
            io.promise_create_batch(&promise);
        }
    }

    #[no_mangle]
    pub extern "C" fn storage_unregister() {
        let mut io = Runtime;
        io.assert_one_yocto().sdk_unwrap();
        let predecessor_account_id = io.predecessor_account_id();
        let force = parse_json(&io.read_input().to_vec()).and_then(|args| args.bool("force").ok());
        let maybe_promise = EthConnectorContract::get_instance(io)
            .storage_unregister(predecessor_account_id, force);
        if let Some(promise) = maybe_promise {
            io.promise_create_batch(&promise);
        }
    }

    #[no_mangle]
    pub extern "C" fn storage_withdraw() {
        let io = Runtime;
        io.assert_one_yocto().sdk_unwrap();
        let args =
            StorageWithdrawCallArgs::from(parse_json(&io.read_input().to_vec()).sdk_unwrap());
        let predecessor_account_id = io.predecessor_account_id();
        EthConnectorContract::get_instance(io).storage_withdraw(&predecessor_account_id, args)
    }

    #[no_mangle]
    pub extern "C" fn storage_balance_of() {
        let io = Runtime;
        EthConnectorContract::get_instance(io).storage_balance_of()
    }

    #[no_mangle]
    pub extern "C" fn get_paused_flags() {
        let mut io = Runtime;
        let paused_flags = EthConnectorContract::get_instance(io).get_paused_flags();
        let data = paused_flags.try_to_vec().expect(ERR_FAILED_PARSE);
        io.return_output(&data[..]);
    }

    #[no_mangle]
    pub extern "C" fn set_paused_flags() {
        let io = Runtime;
        io.assert_private_call().sdk_unwrap();

        let args: PauseEthConnectorCallArgs = io.read_input_borsh().sdk_unwrap();
        EthConnectorContract::get_instance(io).set_paused_flags(args);
    }

    #[no_mangle]
    pub extern "C" fn get_accounts_counter() {
        let io = Runtime;
        EthConnectorContract::get_instance(io).get_accounts_counter();
    }

    #[no_mangle]
    pub extern "C" fn get_erc20_from_nep141() {
        let mut io = Runtime;
        let args: GetErc20FromNep141CallArgs = io.read_input_borsh().sdk_unwrap();

        io.return_output(
            engine::get_erc20_from_nep141(&io, &args.nep141)
                .sdk_unwrap()
                .as_slice(),
        );
    }

    #[no_mangle]
    pub extern "C" fn get_nep141_from_erc20() {
        let mut io = Runtime;
        let erc20_address: crate::engine::ERC20Address =
            io.read_input().to_vec().try_into().sdk_unwrap();
        io.return_output(
            engine::nep141_erc20_map(io)
                .lookup_right(&erc20_address)
                .sdk_expect("ERC20_NOT_FOUND")
                .as_ref(),
        );
    }

    #[no_mangle]
    pub extern "C" fn ft_metadata() {
        let mut io = Runtime;
        let metadata: FungibleTokenMetadata =
            EthConnectorContract::get_metadata(&io).unwrap_or_default();
        let json_data = crate::json::JsonValue::from(metadata);
        io.return_output(json_data.to_string().as_bytes())
    }

    #[cfg(feature = "integration-test")]
    #[no_mangle]
    pub extern "C" fn verify_log_entry() {
        sdk::log!("Call from verify_log_entry");
        let mut io = Runtime;
        let data = true.try_to_vec().unwrap();
        io.return_output(&data[..]);
    }

    /// Function used to create accounts for tests
    #[cfg(feature = "integration-test")]
    #[no_mangle]
    pub extern "C" fn mint_account() {
        use evm::backend::ApplyBackend;
        const GAS_FOR_VERIFY: u64 = 20_000_000_000_000;
        const GAS_FOR_FINISH: u64 = 50_000_000_000_000;

        let mut io = Runtime;
        let args: ([u8; 20], u64, u64) = io.read_input_borsh().sdk_expect("ERR_ARGS");
        let address = Address(args.0);
        let nonce = U256::from(args.1);
        let balance = U256::from(args.2);
        let current_account_id = io.current_account_id();
        let mut engine = Engine::new(address, current_account_id, io, &io).sdk_unwrap();
        let state_change = evm::backend::Apply::Modify {
            address,
            basic: evm::backend::Basic { balance, nonce },
            code: None,
            storage: core::iter::empty(),
            reset_storage: false,
        };
        engine.apply(core::iter::once(state_change), core::iter::empty(), false);

        // Call "finish_deposit" to mint the corresponding
        // nETH NEP-141 tokens as well
        let aurora_account_id = io.current_account_id();
        let args = crate::parameters::FinishDepositCallArgs {
            new_owner_id: aurora_account_id.clone(),
            amount: balance.low_u128(),
            proof_key: crate::prelude::String::new(),
            relayer_id: aurora_account_id.clone(),
            fee: 0,
            msg: None,
        };
        let verify_call = aurora_engine_types::parameters::PromiseCreateArgs {
            target_account_id: aurora_account_id.clone(),
            method: "verify_log_entry".to_string(),
            args: Vec::new(),
            attached_balance: 0,
            attached_gas: GAS_FOR_VERIFY,
        };
        let finish_call = aurora_engine_types::parameters::PromiseCreateArgs {
            target_account_id: aurora_account_id,
            method: "finish_deposit".to_string(),
            args: args.try_to_vec().unwrap(),
            attached_balance: 0,
            attached_gas: GAS_FOR_FINISH,
        };
        io.promise_crate_with_callback(&aurora_engine_types::parameters::PromiseWithCallbackArgs {
            base: verify_call,
            callback: finish_call,
        });
    }

    ///
    /// Utility methods.
    ///

    fn internal_get_upgrade_index() -> u64 {
        let io = Runtime;
        match io.read_u64(&bytes_to_key(KeyPrefix::Config, CODE_STAGE_KEY)) {
            Ok(index) => index,
            Err(sdk::error::ReadU64Error::InvalidU64) => sdk::panic_utf8(b"ERR_INVALID_UPGRADE"),
            Err(sdk::error::ReadU64Error::MissingValue) => sdk::panic_utf8(b"ERR_NO_UPGRADE"),
        }
    }

    fn require_owner_only(state: &EngineState, predecessor_account_id: &AccountId) {
        if &state.owner_id != predecessor_account_id {
            sdk::panic_utf8(b"ERR_NOT_ALLOWED");
        }
    }

    fn predecessor_address(predecessor_account_id: &AccountId) -> Address {
        near_account_to_evm_address(predecessor_account_id.as_bytes())
    }
}

pub trait AuroraState {
    fn add_promise(&mut self, promise: PromiseCreateArgs);
}
