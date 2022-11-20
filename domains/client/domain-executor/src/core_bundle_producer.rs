#![allow(unused)]
use crate::bundle_election_solver::BundleElectionSolver;
use crate::domain_bundle_producer::ReceiptInterface;
use crate::domain_bundle_proposer::DomainBundleProposer;
use crate::utils::ExecutorSlotInfo;
use crate::{BundleSender, ExecutionReceiptFor};
use codec::{Decode, Encode};
use domain_runtime_primitives::{AccountId, DomainCoreApi};
use futures::{select, FutureExt};
use sc_client_api::{AuxStore, BlockBackend, ProofProvider};
use sc_transaction_pool_api::InPoolTransaction;
use sp_api::{NumberFor, ProvideRuntimeApi};
use sp_block_builder::BlockBuilder;
use sp_blockchain::HeaderBackend;
use sp_consensus_slots::Slot;
use sp_domains::{
    Bundle, BundleHeader, DomainId, ExecutorPublicKey, ExecutorSignature, ProofOfElection,
    SignedBundle, SignedOpaqueBundle,
};
use sp_keystore::{SyncCryptoStore, SyncCryptoStorePtr};
use sp_runtime::generic::BlockId;
use sp_runtime::traits::{BlakeTwo256, Block as BlockT, Hash as HashT, Header as HeaderT, Zero};
use sp_runtime::RuntimeAppPublic;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time;
use subspace_core_primitives::BlockNumber;
use system_runtime_primitives::SystemDomainApi;

const LOG_TARGET: &str = "bundle-producer";

pub(super) struct CoreBundleProducer<Block, SBlock, PBlock, Client, SClient, TransactionPool>
where
    Block: BlockT,
    SBlock: BlockT,
    PBlock: BlockT,
{
    domain_id: DomainId,
    system_domain_client: Arc<SClient>,
    client: Arc<Client>,
    bundle_sender: Arc<BundleSender<Block, PBlock>>,
    is_authority: bool,
    keystore: SyncCryptoStorePtr,
    bundle_election_solver: BundleElectionSolver<SBlock, PBlock, SClient>,
    domain_bundle_proposer: DomainBundleProposer<Block, Client, TransactionPool>,
    _phantom_data: PhantomData<(SBlock, PBlock)>,
}

impl<Block, SBlock, PBlock, Client, SClient, TransactionPool> Clone
    for CoreBundleProducer<Block, SBlock, PBlock, Client, SClient, TransactionPool>
where
    Block: BlockT,
    SBlock: BlockT,
    PBlock: BlockT,
{
    fn clone(&self) -> Self {
        Self {
            domain_id: self.domain_id,
            system_domain_client: self.system_domain_client.clone(),
            client: self.client.clone(),
            bundle_sender: self.bundle_sender.clone(),
            is_authority: self.is_authority,
            keystore: self.keystore.clone(),
            bundle_election_solver: self.bundle_election_solver.clone(),
            domain_bundle_proposer: self.domain_bundle_proposer.clone(),
            _phantom_data: self._phantom_data,
        }
    }
}

impl<Block, SBlock, PBlock, Client, SClient, TransactionPool> ReceiptInterface<SBlock::Hash>
    for CoreBundleProducer<Block, SBlock, PBlock, Client, SClient, TransactionPool>
where
    Block: BlockT,
    SBlock: BlockT,
    PBlock: BlockT,
    Client: HeaderBackend<Block> + BlockBackend<Block> + AuxStore + ProvideRuntimeApi<Block>,
    Client::Api: BlockBuilder<Block>,
    SClient: HeaderBackend<SBlock> + ProvideRuntimeApi<SBlock> + ProofProvider<SBlock>,
    SClient::Api:
        DomainCoreApi<SBlock, AccountId> + SystemDomainApi<SBlock, NumberFor<PBlock>, PBlock::Hash>,
    TransactionPool: sc_transaction_pool_api::TransactionPool<Block = Block>,
{
    fn best_execution_chain_number(
        &self,
        at: SBlock::Hash,
    ) -> Result<BlockNumber, sp_api::ApiError> {
        let best_execution_chain_number = self
            .system_domain_client
            .runtime_api()
            .best_execution_chain_number(&BlockId::Hash(at), self.domain_id)?;

        let best_execution_chain_number: BlockNumber = best_execution_chain_number
            .try_into()
            .unwrap_or_else(|_| panic!("Primary number must fit into u32; qed"));

        Ok(best_execution_chain_number)
    }

    fn maximum_receipt_drift(&self, at: SBlock::Hash) -> Result<BlockNumber, sp_api::ApiError> {
        // Receipts for some previous blocks are missing.
        let max_drift = self
            .system_domain_client
            .runtime_api()
            .maximum_receipt_drift(&BlockId::Hash(at))?;

        let max_drift: BlockNumber = max_drift
            .try_into()
            .unwrap_or_else(|_| panic!("Primary number must fit into u32; qed"));

        Ok(max_drift)
    }
}

impl<Block, SBlock, PBlock, Client, SClient, TransactionPool>
    CoreBundleProducer<Block, SBlock, PBlock, Client, SClient, TransactionPool>
where
    Block: BlockT,
    SBlock: BlockT,
    PBlock: BlockT,
    Client: HeaderBackend<Block> + BlockBackend<Block> + AuxStore + ProvideRuntimeApi<Block>,
    Client::Api: BlockBuilder<Block>,
    SClient: HeaderBackend<SBlock> + ProvideRuntimeApi<SBlock> + ProofProvider<SBlock>,
    SClient::Api:
        DomainCoreApi<SBlock, AccountId> + SystemDomainApi<SBlock, NumberFor<PBlock>, PBlock::Hash>,
    TransactionPool: sc_transaction_pool_api::TransactionPool<Block = Block>,
{
    pub(super) fn new(
        domain_id: DomainId,
        system_domain_client: Arc<SClient>,
        client: Arc<Client>,
        transaction_pool: Arc<TransactionPool>,
        bundle_sender: Arc<BundleSender<Block, PBlock>>,
        is_authority: bool,
        keystore: SyncCryptoStorePtr,
    ) -> Self {
        let bundle_election_solver = BundleElectionSolver::<SBlock, PBlock, SClient>::new(
            system_domain_client.clone(),
            keystore.clone(),
        );
        let domain_bundle_proposer = DomainBundleProposer::new(client.clone(), transaction_pool);
        Self {
            domain_id,
            system_domain_client,
            client,
            bundle_sender,
            is_authority,
            keystore,
            bundle_election_solver,
            domain_bundle_proposer,
            _phantom_data: PhantomData::default(),
        }
    }

    pub(super) async fn produce_bundle<R>(
        self,
        primary_info: (PBlock::Hash, NumberFor<PBlock>),
        slot_info: ExecutorSlotInfo,
        receipt_interface: R,
    ) -> Result<
        Option<SignedOpaqueBundle<NumberFor<PBlock>, PBlock::Hash, Block::Hash>>,
        sp_blockchain::Error,
    >
    where
        R: ReceiptInterface<SBlock::Hash>,
    {
        let ExecutorSlotInfo {
            slot,
            global_challenge,
        } = slot_info;

        let best_hash = self.system_domain_client.info().best_hash;
        let best_number = self.system_domain_client.info().best_number;

        if let Some(proof_of_election) = self
            .bundle_election_solver
            .solve_bundle_election_challenge(
                best_hash,
                best_number,
                self.domain_id,
                global_challenge,
            )?
        {
            tracing::info!(target: LOG_TARGET, "📦 Claimed bundle at slot {slot}");

            let bundle = self
                .domain_bundle_proposer
                .propose_bundle_at::<PBlock, _, _>(slot, primary_info, receipt_interface, best_hash)
                .await?;

            let to_sign = bundle.hash();

            match SyncCryptoStore::sign_with(
                &*self.keystore,
                ExecutorPublicKey::ID,
                &proof_of_election.executor_public_key.clone().into(),
                to_sign.as_ref(),
            ) {
                Ok(Some(signature)) => {
                    let best_hash = self.client.info().best_hash;

                    let as_core_block_hash = |system_block_hash: SBlock::Hash| {
                        Block::Hash::decode(&mut system_block_hash.encode().as_slice()).unwrap()
                    };

                    let signed_bundle = SignedBundle {
                        bundle,
                        proof_of_election: ProofOfElection {
                            domain_id: proof_of_election.domain_id,
                            vrf_output: proof_of_election.vrf_output,
                            vrf_proof: proof_of_election.vrf_proof,
                            executor_public_key: proof_of_election.executor_public_key,
                            global_challenge: proof_of_election.global_challenge,
                            state_root: as_core_block_hash(proof_of_election.state_root),
                            storage_proof: proof_of_election.storage_proof,
                            block_number: proof_of_election.block_number,
                            block_hash: as_core_block_hash(proof_of_election.block_hash),
                            // TODO: override the core block info, see if there is a nicer way
                            // later.
                            core_block_hash: Some(best_hash),
                            core_state_root: Some(
                                *self
                                    .client
                                    .header(BlockId::Hash(best_hash))?
                                    .expect("Best block header must exist; qed")
                                    .state_root(),
                            ),
                        },
                        signature: ExecutorSignature::decode(&mut signature.as_slice()).map_err(
                            |err| {
                                sp_blockchain::Error::Application(Box::from(format!(
                                    "Failed to decode the signature of bundle: {err}"
                                )))
                            },
                        )?,
                    };

                    // TODO: Re-enable the bundle gossip over X-Net when the compact bundle is supported.
                    // if let Err(e) = self.bundle_sender.unbounded_send(signed_bundle.clone()) {
                    // tracing::error!(target: LOG_TARGET, error = ?e, "Failed to send transaction bundle");
                    // }

                    Ok(Some(signed_bundle.into_signed_opaque_bundle()))
                }
                Ok(None) => Err(sp_blockchain::Error::Application(Box::from(
                    "This should not happen as the existence of key was just checked",
                ))),
                Err(error) => Err(sp_blockchain::Error::Application(Box::from(format!(
                    "Error occurred when signing the bundle: {error}"
                )))),
            }
        } else {
            Ok(None)
        }
    }
}
