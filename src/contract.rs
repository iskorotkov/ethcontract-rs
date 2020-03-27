//! Abtraction for interacting with ethereum smart contracts. Provides methods
//! for sending transactions to contracts as well as querying current contract
//! state.

mod deploy;
mod deployed;
mod event;
mod method;

use crate::abicompat::AbiCompat;
use crate::errors::{DeployError, LinkError};
use crate::log::LogStream;
use ethcontract_common::abi::{Error as AbiError, Result as AbiResult};
use ethcontract_common::abiext::FunctionExt;
use ethcontract_common::truffle::Network;
use ethcontract_common::{Abi, Artifact, Bytecode};
use std::collections::HashMap;
use web3::api::Web3;
use web3::contract::tokens::{Detokenize, Tokenize};
use web3::types::{Address, Bytes, FilterBuilder};
use web3::Transport;

pub use self::deploy::{Deploy, DeployBuilder, DeployFuture};
pub use self::deployed::{DeployedFuture, FromNetwork};
pub use self::event::{Event, EventBuilder, EventStream, Topic, DEFAULT_POLL_INTERVAL};
pub use self::method::{
    CallFuture, MethodBuilder, MethodDefaults, MethodFuture, MethodSendFuture, ViewMethodBuilder,
};

/// Represents a contract instance at an address. Provides methods for
/// contract interaction.
#[derive(Debug, Clone)]
pub struct Instance<T: Transport> {
    web3: Web3<T>,
    abi: Abi,
    address: Address,
    /// Default method parameters to use when sending method transactions or
    /// querying method calls.
    pub defaults: MethodDefaults,
    /// A mapping from method signature to a name-index pair for accessing
    /// functions in the contract ABI. This is used to avoid allocation when
    /// searching for matching functions by signature.
    methods: HashMap<String, (String, usize)>,
}

impl<T: Transport> Instance<T> {
    /// Creates a new contract instance with the specified `web3` provider with
    /// the given `Abi` at the given `Address`.
    ///
    /// Note that this does not verify that a contract with a matchin `Abi` is
    /// actually deployed at the given address.
    pub fn at(web3: Web3<T>, abi: Abi, address: Address) -> Self {
        let methods = abi
            .functions
            .iter()
            .flat_map(|(name, functions)| {
                functions.iter().enumerate().map(move |(index, function)| {
                    (function.abi_signature(), (name.to_owned(), index))
                })
            })
            .collect();
        Instance {
            web3,
            abi,
            address,
            defaults: MethodDefaults::default(),
            methods,
        }
    }

    /// Locates a deployed contract based on the current network ID reported by
    /// the `web3` provider from the given `Artifact`'s ABI and networks.
    ///
    /// Note that this does not verify that a contract with a matchin `Abi` is
    /// actually deployed at the given address.
    pub fn deployed(web3: Web3<T>, artifact: Artifact) -> DeployedFuture<T, Self> {
        DeployedFuture::new(web3, Deployments::new(artifact))
    }

    /// Creates a contract builder with the specified `web3` provider and the
    /// given `Artifact` byte code. This allows the contract deployment
    /// transaction to be configured before deploying the contract.
    pub fn builder<P>(
        web3: Web3<T>,
        artifact: Artifact,
        params: P,
    ) -> Result<DeployBuilder<T, Self>, DeployError>
    where
        P: Tokenize,
    {
        Linker::new(artifact).deploy(web3, params)
    }

    /// Deploys a contract with the specified `web3` provider with the given
    /// `Artifact` byte code and linking libraries.
    pub fn link_and_deploy<'a, P, I>(
        web3: Web3<T>,
        artifact: Artifact,
        params: P,
        libraries: I,
    ) -> Result<DeployBuilder<T, Self>, DeployError>
    where
        P: Tokenize,
        I: Iterator<Item = (&'a str, Address)>,
    {
        let mut linker = Linker::new(artifact);
        for (name, address) in libraries {
            linker = linker.library(name, address)?;
        }

        linker.deploy(web3, params)
    }

    /// Retrieve the underlying web3 provider used by this contract instance.
    pub fn web3(&self) -> Web3<T> {
        self.web3.clone()
    }

    /// Retrieves the contract ABI for this instance.
    pub fn abi(&self) -> &Abi {
        &self.abi
    }

    /// Returns the contract address being used by this instance.
    pub fn address(&self) -> Address {
        self.address
    }

    /// Returns a method builder to setup a call or transaction on a smart
    /// contract method. Note that calls just get evaluated on a node but do not
    /// actually commit anything to the block chain.
    pub fn method<S, P, R>(&self, signature: S, params: P) -> AbiResult<MethodBuilder<T, R>>
    where
        S: AsRef<str>,
        P: Tokenize,
    {
        let signature = signature.as_ref();
        let function = self
            .methods
            .get(signature)
            .map(|(name, index)| &self.abi.functions[name][*index])
            .ok_or_else(|| AbiError::InvalidName(signature.into()))?;
        let data = function.encode_input(&params.into_tokens().compat())?;

        // take ownership here as it greatly simplifies dealing with futures
        // lifetime as it would require the contract Instance to live until
        // the end of the future
        let function = function.clone();
        let data = Bytes(data);

        Ok(
            MethodBuilder::new(self.web3(), function, self.address, data)
                .with_defaults(&self.defaults),
        )
    }

    /// Returns a view method builder to setup a call to a smart contract. View
    /// method builders can't actually send transactions and only query contract
    /// state.
    pub fn view_method<S, P, R>(
        &self,
        signature: S,
        params: P,
    ) -> AbiResult<ViewMethodBuilder<T, R>>
    where
        S: AsRef<str>,
        P: Tokenize,
        R: Detokenize,
    {
        Ok(self.method(signature, params)?.view())
    }

    /// Returns a event builder to setup an event stream for a smart contract
    /// that emits events for the specified Solidity event by name.
    pub fn event<S, E>(&self, name: S) -> AbiResult<EventBuilder<T, E>>
    where
        S: AsRef<str>,
        E: Detokenize,
    {
        let event = self.abi.event(name.as_ref())?;

        Ok(EventBuilder::new(
            self.web3(),
            event.clone(),
            self.address(),
        ))
    }

    /// Returns a log stream that emits a log for every new event emitted after
    /// the stream was created for this contract instance.
    pub fn all_events(&self) -> LogStream<T> {
        let filter = FilterBuilder::default().address(vec![self.address]).build();
        LogStream::new(self.web3(), filter, DEFAULT_POLL_INTERVAL)
    }
}

/// Deployment information for for an `Instance`. This includes the contract ABI
/// and the known addresses of contracts for network IDs.
/// be used directly but rather through the `Instance::deployed` API.
#[derive(Debug, Clone)]
pub struct Deployments {
    abi: Abi,
    networks: HashMap<String, Network>,
}

impl Deployments {
    /// Create a new `Deployments` instanced for a contract artifact.
    pub fn new(artifact: Artifact) -> Self {
        Deployments {
            abi: artifact.abi,
            networks: artifact.networks,
        }
    }
}

impl<T: Transport> FromNetwork<T> for Instance<T> {
    type Context = Deployments;

    fn from_network(web3: Web3<T>, network_id: &str, cx: Self::Context) -> Option<Self> {
        let address = cx.networks.get(network_id)?.address;
        Some(Instance::at(web3, cx.abi, address))
    }
}

/// Builder for specifying linking options for a contract.
#[derive(Debug, Clone)]
pub struct Linker {
    /// The contract ABI.
    abi: Abi,
    /// The deployment code for the contract.
    bytecode: Bytecode,
}

impl Linker {
    /// Create a new linker for a contract artifact.
    pub fn new(artifact: Artifact) -> Linker {
        Linker {
            abi: artifact.abi,
            bytecode: artifact.bytecode,
        }
    }

    /// Specify a linked library used for this contract. Note that we
    /// incrementally link so that we can verify each time a library is linked
    /// whether it was successful or not.
    ///
    /// # Panics
    ///
    /// Panics if an invalid library name is used (for example if it is more
    /// than 38 characters long).
    pub fn library<S>(mut self, name: S, address: Address) -> Result<Linker, LinkError>
    where
        S: AsRef<str>,
    {
        self.bytecode.link(name, address)?;
        Ok(self)
    }

    /// Finish linking and check if there are any outstanding unlinked libraries
    /// and create a deployment builder.
    pub fn deploy<T, P>(
        self,
        web3: Web3<T>,
        params: P,
    ) -> Result<DeployBuilder<T, Instance<T>>, DeployError>
    where
        T: Transport,
        P: Tokenize,
    {
        DeployBuilder::new(web3, self, params)
    }
}

impl<T: Transport> Deploy<T> for Instance<T> {
    type Context = Linker;

    fn abi(cx: &Self::Context) -> &Abi {
        &cx.abi
    }

    fn bytecode(cx: &Self::Context) -> &Bytecode {
        &cx.bytecode
    }

    fn at_address(web3: Web3<T>, address: Address, cx: Self::Context) -> Self {
        Instance::at(web3, cx.abi, address)
    }
}
