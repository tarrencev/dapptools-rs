use crate::Evm;

use ethers::types::{Address, Bytes, U256};

use sputnik::{
    backend::{Backend, MemoryAccount},
    executor::{MemoryStackState, StackExecutor, StackState, StackSubstateMetadata},
    Config, ExitReason, Handler,
};
use std::collections::BTreeMap;

use eyre::Result;

pub type MemoryState = BTreeMap<Address, MemoryAccount>;

// TODO: Check if we can implement this as the base layer of an ethers-provider
// Middleware stack instead of doing RPC calls.
pub struct Executor<'a, S> {
    pub executor: StackExecutor<'a, S>,
    pub gas_limit: u64,
}

// Manual implementation of `Clone` for Clone-able StackStates (typically when the Backend
// behind them is also clone-able). This is useful to have e.g. when running fuzz
// tests which we need to take ownership of the EVM and clone it for each run in the
// test runner's closure.
impl<'a, S: StackState<'a> + Clone> Clone for Executor<'a, S> {
    fn clone(&self) -> Self {
        Self {
            gas_limit: self.gas_limit,
            executor: StackExecutor::new_with_precompile(
                self.executor.state().clone(),
                self.executor.config(),
                Default::default(),
            ),
        }
    }
}

// Concrete implementation over the in-memory backend
impl<'a, B: Backend> Executor<'a, MemoryStackState<'a, 'a, B>> {
    /// Given a gas limit, vm version, initial chain configuration and initial state
    // TOOD: See if we can make lifetimes better here
    pub fn new(gas_limit: u64, config: &'a Config, backend: &'a B) -> Self {
        // setup gasometer
        let metadata = StackSubstateMetadata::new(gas_limit, config);
        // setup state
        let state = MemoryStackState::new(metadata, backend);
        // setup executor
        let executor = StackExecutor::new_with_precompile(state, config, Default::default());

        Self { executor, gas_limit }
    }
}

// Note regarding usage of Generic vs Associated Types in traits:
//
// We use StackState as a trait and not as an associated type because we want to
// allow the developer what the db type should be. Whereas for ReturnReason, we want it
// to be generic across implementations, but we don't want to make it a user-controlled generic.
impl<'a, S> Evm<S> for Executor<'a, S>
where
    S: StackState<'a>,
{
    type ReturnReason = ExitReason;

    fn is_success(reason: &Self::ReturnReason) -> bool {
        matches!(reason, ExitReason::Succeed(_))
    }

    fn is_fail(reason: &Self::ReturnReason) -> bool {
        matches!(reason, ExitReason::Revert(_))
    }

    fn reset(&mut self, state: S) {
        let mut _state = self.executor.state_mut();
        *_state = state;
    }

    /// given an iterator of contract address to contract bytecode, initializes
    /// the state with the contract deployed at the specified address
    fn initialize_contracts<T: IntoIterator<Item = (Address, Bytes)>>(&mut self, contracts: T) {
        let state_ = self.executor.state_mut();
        contracts.into_iter().for_each(|(address, bytecode)| {
            state_.set_code(address, bytecode.to_vec());
        })
    }

    fn state(&self) -> &S {
        self.executor.state()
    }

    /// Runs the selected function
    fn call_raw(
        &mut self,
        from: Address,
        to: Address,
        calldata: Bytes,
        value: U256,
        _is_static: bool,
    ) -> Result<(Bytes, ExitReason, u64)> {
        let gas_before = self.executor.gas_left();

        let (status, retdata) =
            self.executor.transact_call(from, to, value, calldata.to_vec(), self.gas_limit, vec![]);

        let gas_after = self.executor.gas_left();
        let gas = dapp_utils::remove_extra_costs(gas_before - gas_after, calldata.as_ref());

        Ok((retdata.into(), status, gas.as_u64()))
    }
}

#[cfg(any(test, feature = "sputnik-helpers"))]
pub mod helpers {
    use super::*;
    use ethers::types::H160;
    use sputnik::backend::{MemoryBackend, MemoryVicinity};

    pub fn new_backend(vicinity: &MemoryVicinity, state: MemoryState) -> MemoryBackend<'_> {
        MemoryBackend::new(vicinity, state)
    }

    pub fn new_vicinity() -> MemoryVicinity {
        MemoryVicinity {
            gas_price: U256::zero(),
            origin: H160::default(),
            block_hashes: Vec::new(),
            block_number: Default::default(),
            block_coinbase: Default::default(),
            block_timestamp: Default::default(),
            block_difficulty: Default::default(),
            block_gas_limit: Default::default(),
            chain_id: U256::one(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        helpers::{new_backend, new_vicinity},
        *,
    };
    use crate::test_helpers::{can_call_vm_directly, solidity_unit_test, COMPILED};
    use dapp_utils::{decode_revert, get_func};

    use ethers::utils::id;
    use sputnik::{ExitReason, ExitRevert, ExitSucceed};

    #[test]
    fn sputnik_can_call_vm_directly() {
        let cfg = Config::istanbul();
        let compiled = COMPILED.get("Greeter").expect("could not find contract");

        let addr = "0x1000000000000000000000000000000000000000".parse().unwrap();

        let vicinity = new_vicinity();
        let backend = new_backend(&vicinity, Default::default());
        let mut evm = Executor::new(12_000_000, &cfg, &backend);
        evm.initialize_contracts(vec![(addr, compiled.runtime_bytecode.clone())]);

        can_call_vm_directly(evm, addr, compiled);
    }

    #[test]
    fn sputnik_solidity_unit_test() {
        let cfg = Config::istanbul();

        let compiled = COMPILED.get("GreeterTest").expect("could not find contract");

        let addr = "0x1000000000000000000000000000000000000000".parse().unwrap();

        let vicinity = new_vicinity();
        let backend = new_backend(&vicinity, Default::default());
        let mut evm = Executor::new(12_000_000, &cfg, &backend);
        evm.initialize_contracts(vec![(addr, compiled.runtime_bytecode.clone())]);

        solidity_unit_test(evm, addr, compiled);
    }

    #[test]
    fn failing_with_no_reason_if_no_setup() {
        let cfg = Config::istanbul();

        let compiled = COMPILED.get("GreeterTest").expect("could not find contract");

        let addr = "0x1000000000000000000000000000000000000000".parse().unwrap();

        let vicinity = new_vicinity();
        let backend = new_backend(&vicinity, Default::default());
        let mut evm = Executor::new(12_000_000, &cfg, &backend);
        evm.initialize_contracts(vec![(addr, compiled.runtime_bytecode.clone())]);

        let (status, res) = evm.executor.transact_call(
            Address::zero(),
            addr,
            0.into(),
            id("testFailGreeting()").to_vec(),
            evm.gas_limit,
            vec![],
        );
        assert_eq!(status, ExitReason::Revert(ExitRevert::Reverted));
        assert!(res.is_empty());
    }

    #[test]
    fn failing_solidity_unit_test() {
        let cfg = Config::istanbul();

        let compiled = COMPILED.get("GreeterTest").expect("could not find contract");

        let addr = "0x1000000000000000000000000000000000000000".parse().unwrap();

        let vicinity = new_vicinity();
        let backend = new_backend(&vicinity, Default::default());
        let mut evm = Executor::new(12_000_000, &cfg, &backend);
        evm.initialize_contracts(vec![(addr, compiled.runtime_bytecode.clone())]);

        // call the setup function to deploy the contracts inside the test
        let (_, status, _) = evm
            .call::<(), _>(
                Address::zero(),
                addr,
                &get_func("function setUp() external").unwrap(),
                (),
                0.into(),
            )
            .unwrap();
        assert_eq!(status, ExitReason::Succeed(ExitSucceed::Stopped));

        let (status, res) = evm.executor.transact_call(
            Address::zero(),
            addr,
            0.into(),
            id("testFailGreeting()").to_vec(),
            evm.gas_limit,
            vec![],
        );
        assert_eq!(status, ExitReason::Revert(ExitRevert::Reverted));
        let reason = decode_revert(&res).unwrap();
        assert_eq!(reason, "not equal to `hi`");
    }
}
