use crate::{artifacts::DapptoolsArtifact, runner::TestResult, ContractRunner};
use dapp_solc::SolcBuilder;
use evm_adapters::Evm;

use ethers::{
    types::Address,
    utils::{keccak256, CompiledContract},
};

use proptest::test_runner::TestRunner;
use regex::Regex;

use eyre::Result;
use std::{collections::HashMap, marker::PhantomData, path::PathBuf};

/// Builder used for instantiating the multi-contract runner
#[derive(Clone, Debug, Default)]
pub struct MultiContractRunnerBuilder<'a> {
    /// Glob to the contracts we want compiled
    pub contracts: &'a str,
    /// Solc remappings
    pub remappings: &'a [String],
    /// Solc lib import paths
    pub libraries: &'a [String],
    /// The path for the output file
    pub out_path: PathBuf,
    pub no_compile: bool,
    /// The fuzzer to be used for running fuzz tests
    pub fuzzer: Option<TestRunner>,
}

impl<'a> MultiContractRunnerBuilder<'a> {
    /// Given an EVM, proceeds to return a runner which is able to execute all tests
    /// against that evm
    pub fn build<E, S>(self, mut evm: E) -> Result<MultiContractRunner<E, S>>
    where
        E: Evm<S>,
    {
        // 1. incremental compilation
        // 2. parallel compilation
        // 3. Hardhat / Truffle-style artifacts
        let contracts = if self.no_compile {
            let out_file = std::fs::read_to_string(&self.out_path)?;
            serde_json::from_str::<DapptoolsArtifact>(&out_file)?.contracts()?
        } else {
            SolcBuilder::new(self.contracts, self.remappings, self.libraries)?.build_all()?
        };

        let mut addresses = HashMap::new();
        let init_state = contracts.iter().map(|(name, compiled)| {
            // make a fake address for the contract, maybe anti-pattern
            let addr = Address::from_slice(&keccak256(&compiled.runtime_bytecode)[..20]);
            addresses.insert(name.clone(), addr);
            (addr, compiled.runtime_bytecode.clone())
        });
        evm.initialize_contracts(init_state);

        Ok(MultiContractRunner {
            contracts,
            addresses,
            evm,
            state: PhantomData,
            fuzzer: self.fuzzer,
        })
    }

    pub fn contracts(mut self, contracts: &'a str) -> Self {
        self.contracts = contracts;
        self
    }

    pub fn fuzzer(mut self, fuzzer: TestRunner) -> Self {
        self.fuzzer = Some(fuzzer);
        self
    }

    pub fn remappings(mut self, remappings: &'a [String]) -> Self {
        self.remappings = remappings;
        self
    }

    pub fn libraries(mut self, libraries: &'a [String]) -> Self {
        self.libraries = libraries;
        self
    }

    pub fn out_path(mut self, out_path: PathBuf) -> Self {
        self.out_path = out_path;
        self
    }

    pub fn skip_compilation(mut self, flag: bool) -> Self {
        self.no_compile = flag;
        self
    }
}

pub struct MultiContractRunner<E, S> {
    /// Mapping of contract name to compiled bytecode
    contracts: HashMap<String, CompiledContract>,
    /// Mapping of contract name to the address it's been injected in the EVM state
    addresses: HashMap<String, Address>,
    /// The EVM instance used in the test runner
    evm: E,
    fuzzer: Option<TestRunner>,
    state: PhantomData<S>,
}

impl<E, S> MultiContractRunner<E, S>
where
    E: Evm<S>,
{
    pub fn test(&mut self, pattern: Regex) -> Result<HashMap<String, HashMap<String, TestResult>>> {
        // NB: We also have access to the contract's abi. When running the test.
        // Can this be useful for decorating the stacktrace during a revert?
        // TODO: Check if the function starts with `prove` or `invariant`
        // Filter out for contracts that have at least 1 test function
        let contracts = std::mem::take(&mut self.contracts);
        let tests = contracts
            .iter()
            .filter(|(_, contract)| contract.abi.functions().any(|x| x.name.starts_with("test")));

        // TODO: Is this pattern OK? We use the memory and then write it back to avoid any
        // borrow checker issues. Otherwise, we'd need to clone large vectors.
        let addresses = std::mem::take(&mut self.addresses);
        let results = tests
            .into_iter()
            .map(|(name, contract)| {
                let address = addresses
                    .get(name)
                    .ok_or_else(|| eyre::eyre!("could not find contract address"))?;

                let result = self.run_tests(name, contract, *address, &pattern)?;
                Ok((name.clone(), result))
            })
            .filter_map(|x: Result<_>| x.ok())
            .filter_map(|(name, res)| if res.is_empty() { None } else { Some((name, res)) })
            .collect::<HashMap<_, _>>();

        self.contracts = contracts;
        self.addresses = addresses;

        Ok(results)
    }

    // The _name field is unused because we only want it for tracing
    #[tracing::instrument(
        name = "contract",
        skip_all,
        err,
        fields(name = %_name)
    )]
    fn run_tests(
        &mut self,
        _name: &str,
        contract: &CompiledContract,
        address: Address,
        pattern: &Regex,
    ) -> Result<HashMap<String, TestResult>> {
        let mut runner = ContractRunner::new(&mut self.evm, contract, address);
        runner.run_tests(pattern, self.fuzzer.as_mut())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_multi_runner<S, E: Evm<S>>(evm: E) {
        let mut runner =
            MultiContractRunnerBuilder::default().contracts("./GreetTest.sol").build(evm).unwrap();

        let results = runner.test(Regex::new(".*").unwrap()).unwrap();

        // 2 contracts
        assert_eq!(results.len(), 2);

        // 3 tests on greeter 1 on gm
        assert_eq!(results["GreeterTest"].len(), 3);
        assert_eq!(results["GmTest"].len(), 1);
        for (_, res) in results {
            assert!(res.iter().all(|(_, result)| result.success));
        }

        let only_gm = runner.test(Regex::new("testGm.*").unwrap()).unwrap();
        assert_eq!(only_gm.len(), 1);
        assert_eq!(only_gm["GmTest"].len(), 1);
    }

    fn test_ds_test_fail<S, E: Evm<S>>(evm: E) {
        let mut runner =
            MultiContractRunnerBuilder::default().contracts("./../FooTest.sol").build(evm).unwrap();
        let results = runner.test(Regex::new(".*").unwrap()).unwrap();
        let test = results.get("FooTest").unwrap().get("testFailX").unwrap();
        assert!(test.success);
    }

    mod sputnik {
        use super::*;
        use evm::Config;
        use evm_adapters::sputnik::{
            helpers::{new_backend, new_vicinity},
            Executor,
        };

        #[test]
        fn test_sputnik_multi_runner() {
            let config = Config::istanbul();
            let gas_limit = 12_500_000;
            let env = new_vicinity();
            let backend = new_backend(&env, Default::default());
            let evm = Executor::new(gas_limit, &config, &backend);
            test_multi_runner(evm);
        }

        #[test]
        fn test_sputnik_ds_test_fail() {
            let config = Config::istanbul();
            let gas_limit = 12_500_000;
            let env = new_vicinity();
            let backend = new_backend(&env, Default::default());
            let evm = Executor::new(gas_limit, &config, &backend);
            test_ds_test_fail(evm);
        }
    }

    // TODO: Add EvmOdin tests once we get the Mocked Host working
}
