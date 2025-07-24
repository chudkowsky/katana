use std::borrow::Cow;
use std::future::Future;

use futures::executor::block_on;
use jsonrpsee::core::middleware;
use jsonrpsee::core::middleware::{Batch, Notification};
use jsonrpsee::core::traits::ToRpcParams;
use jsonrpsee::types::Request;
use katana_executor::ExecutorFactory;
use katana_pool::{TransactionPool, TxPool};
use katana_primitives::block::{BlockIdOrTag, BlockTag};
use katana_primitives::chain::ChainId;
use katana_primitives::fee::{AllResourceBoundsMapping, ResourceBoundsMapping};
use katana_primitives::genesis::constant::DEFAULT_UDC_ADDRESS;
use katana_primitives::transaction::{ExecutableTx, ExecutableTxWithHash, InvokeTx, InvokeTxV3};
use katana_primitives::{ContractAddress, Felt};
use katana_rpc::starknet::StarknetApi;
use katana_rpc_api::error::starknet::StarknetApiError;
use katana_rpc_types::transaction::{BroadcastedInvokeTx, BroadcastedTx};
use starknet::core::types::{Call, SimulationFlagForEstimateFee};
use starknet::macros::selector;
use starknet::signers::{LocalWallet, Signer, SigningKey};

use crate::rpc::types::OutsideExecution;
use crate::utils::encode_calls;
use crate::Client;

#[derive(Debug, Clone)]
pub struct Paymaster<S, EF: ExecutorFactory> {
    service: S,
    starknet_api: StarknetApi<EF>,
    cartridge_api: Client,
    paymaster_address: ContractAddress,
    paymaster_key: SigningKey,
    pool: TxPool,
    chain_id: ChainId,
}

impl<S, EF: ExecutorFactory> Paymaster<S, EF> {
    pub fn intercept_estimate_fee<'a>(&self, request: &Request<'a>) -> Request<'a> {
        let params = request.params();

        let (mut requests, simulation_flags, block_id) = if params.is_object() {
            #[derive(serde::Deserialize)]
            // #[serde(crate = "jsonrpsee :: core :: __reexports :: serde")]
            struct ParamsObject<G0, G1, G2> {
                request: G0,
                #[serde(alias = "simulationFlags")]
                simulation_flags: G1,
                #[serde(alias = "blockId")]
                block_id: G2,
            }

            let parsed: ParamsObject<
                Vec<BroadcastedTx>,
                Vec<SimulationFlagForEstimateFee>,
                BlockIdOrTag,
            > = match params.parse() {
                Ok(p) => p,
                Err(e) => {
                    jsonrpsee::core::__reexports::log_fail_parse_as_object(&e);
                    // return jsonrpsee::ResponsePayload::error(e);
                    todo!()
                }
            };
            (parsed.request, parsed.simulation_flags, parsed.block_id)
        } else {
            let mut seq = params.sequence();
            let request: Vec<BroadcastedTx> = match seq.next() {
                Ok(v) => v,
                Err(e) => {
                    jsonrpsee::core::__reexports::log_fail_parse(
                        "request",
                        "Vec < BroadcastedTx >",
                        &e,
                        false,
                    );
                    // return jsonrpsee::ResponsePayload::error(e);
                    todo!()
                }
            };
            let simulation_flags: Vec<SimulationFlagForEstimateFee> = match seq.next() {
                Ok(v) => v,
                Err(e) => {
                    jsonrpsee::core::__reexports::log_fail_parse(
                        "simulation_flags",
                        "Vec < SimulationFlagForEstimateFee >",
                        &e,
                        false,
                    );
                    // return jsonrpsee::ResponsePayload::error(e);
                    todo!()
                }
            };
            let block_id: BlockIdOrTag = match seq.next() {
                Ok(v) => v,
                Err(e) => {
                    jsonrpsee::core::__reexports::log_fail_parse(
                        "block_id",
                        "BlockIdOrTag",
                        &e,
                        false,
                    );
                    // return jsonrpsee::ResponsePayload::error(e);
                    todo!()
                }
            };
            (request, simulation_flags, block_id)
        };

        let mut new_requests = Vec::with_capacity(requests.len());

        for request in &mut requests {
            // The whole Cartridge paymaster flow would only be accessible mainly from the
            // Controller wallet. The Controller wallet only supports V3 transactions
            // (considering < V3 transactions will soon be deprecated) hence why we're
            // only checking for V3 transactions here.
            //
            // Yes, ideally it's better to handle all versions but it's probably fine for now.
            if let BroadcastedTx::Invoke(BroadcastedInvokeTx(tx)) = &request {
                let deploy_controller_tx = self
                    .get_controller_deploy_tx_if_controller_address(tx.sender_address, block_id)
                    .unwrap();

                if let Some(tx) = deploy_controller_tx {
                    new_requests.push(tx);
                }
            }
        }

        let params = {
            let mut params = jsonrpsee::core::params::ArrayParams::new();

            if let Err(err) = params.insert(requests) {
                jsonrpsee::core::__reexports::panic_fail_serialize("request", err);
            }
            if let Err(err) = params.insert(simulation_flags) {
                jsonrpsee::core::__reexports::panic_fail_serialize("simulation_flags", err);
            }
            if let Err(err) = params.insert(block_id) {
                jsonrpsee::core::__reexports::panic_fail_serialize("block_id", err);
            }

            params
        };

        let mut new_request = request.clone();
        let params = params.to_rpc_params().unwrap();
        let params = params.map(Cow::Owned);
        new_request.params = params;

        new_request
    }

    pub fn intercept_add_outside_execution<'a>(&self, request: &Request<'a>) {
        let params = request.params();

        let (address, ..) = if params.is_object() {
            #[derive(serde::Deserialize)]
            struct ParamsObject<G0, G1, G2> {
                address: G0,
                #[serde(alias = "outsideExecution")]
                outside_execution: G1,
                signature: G2,
            }
            let parsed: ParamsObject<ContractAddress, OutsideExecution, Vec<Felt>> =
                match params.parse() {
                    Ok(p) => p,
                    Err(e) => {
                        jsonrpsee::core::__reexports::log_fail_parse_as_object(&e);
                        return;
                    }
                };
            (parsed.address, parsed.outside_execution, parsed.signature)
        } else {
            let mut seq = params.sequence();
            let address: ContractAddress = match seq.next() {
                Ok(v) => v,
                Err(e) => {
                    jsonrpsee::core::__reexports::log_fail_parse(
                        "address",
                        "ContractAddress",
                        &e,
                        false,
                    );
                    return;
                }
            };
            let outside_execution: OutsideExecution = match seq.next() {
                Ok(v) => v,
                Err(e) => {
                    jsonrpsee::core::__reexports::log_fail_parse(
                        "outside_execution",
                        "OutsideExecution",
                        &e,
                        false,
                    );
                    return;
                }
            };
            let signature: Vec<Felt> = match seq.next() {
                Ok(v) => v,
                Err(e) => {
                    jsonrpsee::core::__reexports::log_fail_parse(
                        "signature",
                        "Vec < Felt >",
                        &e,
                        false,
                    );
                    return;
                }
            };
            (address, outside_execution, signature)
        };

        if let Some(deploy_controller_tx) = self
            .get_controller_deploy_tx_if_controller_address(
                *address,
                BlockIdOrTag::Tag(BlockTag::Pending),
            )
            .unwrap()
        {
            self.pool
                .add_transaction(deploy_controller_tx)
                .expect("failed to add transaction to pool");
        }
    }

    /// Handles the deployment of a cartridge controller if the estimate fee is requested for a
    /// cartridge controller.
    ///
    /// The controller accounts are created with a specific version of the controller.
    /// To ensure address determinism, the controller account must be deployed with the same
    /// version, which is included in the calldata retrieved from the Cartridge API.
    fn get_controller_deploy_tx_if_controller_address(
        &self,
        address: Felt,
        block_id: BlockIdOrTag,
    ) -> anyhow::Result<Option<ExecutableTxWithHash>> {
        // Avoid deploying the controller account if it is already deployed.
        if block_on(self.starknet_api.class_hash_at_address(block_id, address.into())).is_ok() {
            return Ok(None);
        }

        if let tx @ Some(..) = self.get_controller_deploy_tx(address.into(), block_id)? {
            // debug!(address = %maybe_controller_address, "Deploying controller account.");
            return Ok(tx);
        }

        Ok(None)
    }

    /// Crafts a deploy controller transaction for a cartridge controller.
    ///
    /// Returns None if the provided `controller_address` is not registered in the Cartridge API.
    fn get_controller_deploy_tx(
        &self,
        address: ContractAddress,
        block_id: BlockIdOrTag,
    ) -> anyhow::Result<Option<ExecutableTxWithHash>> {
        if let Some(res) = block_on(self.cartridge_api.get_account_calldata(address)).unwrap() {
            // Check if any of the transactions are sent from an address associated with a Cartridge
            // Controller account. If yes, we craft a Controller deployment transaction
            // for each of the unique sender and push it at the beginning of the
            // transaction list so that all the requested transactions are executed against a state
            // with the Controller accounts deployed.
            let paymaster_nonce =
                match block_on(self.starknet_api.nonce_at(block_id, self.paymaster_address)) {
                    Ok(nonce) => nonce,
                    Err(StarknetApiError::ContractNotFound) => {
                        panic!("Cartridge paymaster account doesn't exist");
                    }
                    _ => {
                        panic!("something")
                    }
                };

            let call = Call {
                to: DEFAULT_UDC_ADDRESS.into(),
                selector: selector!("deployContract"),
                calldata: res.constructor_calldata,
            };

            let mut tx = InvokeTxV3 {
                tip: 0_u64,
                signature: vec![],
                nonce: paymaster_nonce,
                paymaster_data: vec![],
                chain_id: self.chain_id,
                account_deployment_data: vec![],
                calldata: encode_calls(vec![call]),
                sender_address: self.paymaster_address,
                nonce_data_availability_mode: katana_primitives::da::DataAvailabilityMode::L1,
                fee_data_availability_mode: katana_primitives::da::DataAvailabilityMode::L1,
                resource_bounds: ResourceBoundsMapping::All(AllResourceBoundsMapping::default()),
            };

            let tx_hash = InvokeTx::V3(tx.clone()).calculate_hash(false);

            let signer = LocalWallet::from(self.paymaster_key.clone());
            let signature = futures::executor::block_on(signer.sign_hash(&tx_hash)).unwrap();
            tx.signature = vec![signature.r, signature.s];

            let tx = ExecutableTxWithHash::new(ExecutableTx::Invoke(InvokeTx::V3(tx)));

            Ok(Some(tx))
        } else {
            Ok(None)
        }
    }
}

/// Paymaster layer.
#[derive(Debug)]
pub struct PaymasterLayer<EF: ExecutorFactory> {
    starknet_api: StarknetApi<EF>,
    cartridge_api: Client,
    paymaster_address: ContractAddress,
    paymaster_key: SigningKey,
    chain_id: ChainId,
    pool: TxPool,
}

impl<EF: ExecutorFactory> PaymasterLayer<EF> {
    pub fn new(
        starknet_api: StarknetApi<EF>,
        cartridge_api: Client,
        paymaster_address: ContractAddress,
        paymaster_key: SigningKey,
        chain_id: ChainId,
        pool: TxPool,
    ) -> Self {
        Self { starknet_api, cartridge_api, paymaster_address, paymaster_key, chain_id, pool }
    }
}

impl<EF: ExecutorFactory> Clone for PaymasterLayer<EF> {
    fn clone(&self) -> Self {
        Self {
            starknet_api: self.starknet_api.clone(),
            cartridge_api: self.cartridge_api.clone(),
            paymaster_address: self.paymaster_address,
            paymaster_key: self.paymaster_key.clone(),
            chain_id: self.chain_id,
            pool: self.pool.clone(),
        }
    }
}

impl<S, EF: ExecutorFactory> tower::Layer<S> for PaymasterLayer<EF> {
    type Service = Paymaster<S, EF>;

    fn layer(&self, service: S) -> Self::Service {
        Paymaster {
            service,
            pool: self.pool.clone(),
            chain_id: self.chain_id,
            starknet_api: self.starknet_api.clone(),
            cartridge_api: self.cartridge_api.clone(),
            paymaster_address: self.paymaster_address,
            paymaster_key: self.paymaster_key.clone(),
        }
    }
}

impl<S, EF> middleware::RpcServiceT for Paymaster<S, EF>
where
    S: middleware::RpcServiceT + Send + Sync + Clone + 'static,
    EF: ExecutorFactory,
{
    type BatchResponse = S::BatchResponse;
    type MethodResponse = S::MethodResponse;
    type NotificationResponse = S::NotificationResponse;

    fn call<'a>(
        &self,
        request: Request<'a>,
    ) -> impl Future<Output = Self::MethodResponse> + Send + 'a {
        if request.method_name() == "starknet_estimateFee" {
            let request = self.intercept_estimate_fee(&request);
            self.service.call(request)
        } else if request.method_name() == "cartridge_addExecuteOutsideTransaction" {
            self.intercept_add_outside_execution(&request);
            self.service.call(request)
        } else {
            self.service.call(request)
        }
    }

    fn batch<'a>(
        &self,
        requests: Batch<'a>,
    ) -> impl Future<Output = Self::BatchResponse> + Send + 'a {
        self.service.batch(requests)
    }

    fn notification<'a>(
        &self,
        n: Notification<'a>,
    ) -> impl Future<Output = Self::NotificationResponse> + Send + 'a {
        self.service.notification(n)
    }
}
