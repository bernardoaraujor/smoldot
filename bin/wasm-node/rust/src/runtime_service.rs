// Smoldot
// Copyright (C) 2019-2021  Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! Background runtime download service.
//!
//! This service plugs on top of a [`sync_service`], listens for new best blocks and checks
//! whether the runtime has changed in any way. Its objective is to always provide an up-to-date
//! [`executor::host::HostVmPrototype`] ready to be called by other services.
//!
//! # Usage
//!
//! The runtime service lets user subscribe to best and finalized block updates, similar to
//! the [`sync_service`]. These subscriptions are implemented by subscribing to the underlying
//! [`sync_service`] and, for each notification, downloading the runtime code of the best or
//! finalized block. Therefore, these notifications always come with a delay compared to directly
//! using the [`sync_service`].
//!
//! Furthermore, if it isn't possible to download the runtime code of a block (for example because
//! peers refuse to answer or have already pruned the block) or if the runtime service already has
//! too many pending downloads, this block is simply skipped and not reported on the
//! subscriptions.
//!
//! Consequently, you are strongly encouraged to not use both the [`sync_service`] *and* the
//! [`RuntimeService`] of the same chain. They each provide a consistent view of the chain, but
//! this view isn't necessarily the same on both services.
//!
//! The main service offered by the runtime service is
//! [`RuntimeService::recent_best_block_runtime_lock`], that performs a runtime call on the latest
//! reported best block or more recent.

use crate::{
    ffi, lossy_channel,
    sync_service::{self, StorageQueryError},
};

use futures::{
    channel::mpsc,
    lock::{Mutex, MutexGuard},
    prelude::*,
};
use smoldot::{
    chain_spec, executor, header,
    informant::HashDisplay,
    metadata,
    network::protocol,
    sync::download_tree,
    trie::{self, proof_verify},
};
use std::{iter, mem, pin::Pin, sync::Arc};

pub use crate::lossy_channel::Receiver as NotificationsReceiver;
pub use smoldot::sync::download_tree::RuntimeError;

/// Configuration for a runtime service.
pub struct Config<'a> {
    /// Name of the chain, for logging purposes.
    ///
    /// > **Note**: This name will be directly printed out. Any special character should already
    /// >           have been filtered out from this name.
    pub log_name: String,

    /// Closure that spawns background tasks.
    pub tasks_executor: Box<dyn FnMut(String, Pin<Box<dyn Future<Output = ()> + Send>>) + Send>,

    /// Service responsible for synchronizing the chain.
    pub sync_service: Arc<sync_service::SyncService>,

    /// Specification of the chain.
    pub chain_spec: &'a chain_spec::ChainSpec,

    /// Header of the genesis block of the chain, in SCALE encoding.
    ///
    /// > **Note**: This can be derived from a [`chain_spec::ChainSpec`]. While the
    /// >           [`RuntimeService::new`] function could in theory use the
    /// >           [`Config::chain_spec`] parameter to derive this value, doing so is quite
    /// >           expensive. We prefer to require this value from the upper layer instead, as
    /// >           it is most likely needed anyway.
    pub genesis_block_scale_encoded_header: Vec<u8>,
}

/// See [the module-level documentation](..).
pub struct RuntimeService {
    /// Target to use for the logs. See [`Config::log_name`].
    log_target: String,

    /// See [`Config::sync_service`].
    sync_service: Arc<sync_service::SyncService>,

    /// Fields behind a `Mutex`. Should only be locked for short-lived operations.
    guarded: Mutex<Guarded>,
}

impl RuntimeService {
    /// Initializes a new runtime service.
    ///
    /// The future returned by this function is expected to finish relatively quickly and is
    /// necessary only for locking purposes.
    pub async fn new(mut config: Config<'_>) -> Arc<Self> {
        // Target to use for all the logs of this service.
        let log_target = format!("runtime-{}", config.log_name);

        let best_near_head_of_chain = config.sync_service.is_near_head_of_chain_heuristic().await;

        // Build the runtime of the genesis block.
        let genesis_runtime = {
            let code = config
                .chain_spec
                .genesis_storage()
                .find(|(k, _)| k == b":code")
                .map(|(_, v)| v.to_vec());
            let heap_pages = config
                .chain_spec
                .genesis_storage()
                .find(|(k, _)| k == b":heappages")
                .map(|(_, v)| v.to_vec());

            // Note that in the absolute we don't need to panic in case of a problem, and could
            // simply store an `Err` and continue running.
            // However, in practice, it seems more sane to detect problems in the genesis block.
            let mut runtime = SuccessfulRuntime::from_params(&code, &heap_pages).await;

            // As documented in the `metadata` field, we must fill it using the genesis storage.
            if let Ok(runtime) = runtime.as_mut() {
                let mut query = metadata::query_metadata(runtime.virtual_machine.take().unwrap());
                loop {
                    match query {
                        metadata::Query::Finished(Ok(metadata), vm) => {
                            runtime.virtual_machine = Some(vm);
                            runtime.metadata = Some(metadata);
                            break;
                        }
                        metadata::Query::StorageGet(get) => {
                            let key = get.key_as_vec();
                            let value = config
                                .chain_spec
                                .genesis_storage()
                                .find(|(k, _)| &**k == key)
                                .map(|(_, v)| v);
                            query = get.inject_value(value.map(iter::once));
                        }
                        metadata::Query::Finished(Err(err), _) => {
                            panic!("Unable to generate genesis metadata: {}", err)
                        }
                    }
                }
            }

            Runtime {
                runtime,
                runtime_code: code,
                heap_pages,
            }
        };

        let runtime_service = Arc::new(RuntimeService {
            log_target,
            sync_service: config.sync_service,
            guarded: Mutex::new(Guarded {
                all_blocks_subscriptions: Vec::new(),
                finalized_blocks_subscriptions: Vec::new(),
                best_blocks_subscriptions: Vec::new(),
                runtime_version_subscriptions: Vec::new(),
                best_near_head_of_chain,
                tree: Some(
                    download_tree::DownloadTree::from_finalized_block_and_runtime(
                        config.genesis_block_scale_encoded_header,
                        genesis_runtime,
                    ),
                ),
            }),
        });

        // Spawns a task that downloads the runtime code at every block to check whether it has
        // changed.
        //
        // This is strictly speaking not necessary as long as there is no active subscription.
        // However, in practice, there is most likely always going to be one. It is way easier to
        // always have a task active rather than create and destroy it.
        (config.tasks_executor)("runtime-download".into(), {
            let runtime_service = runtime_service.clone();
            async move {
                run_background(runtime_service).await;
            }
            .boxed()
        });

        runtime_service
    }

    /// Returns the current runtime version, plus an unlimited stream that produces one item every
    /// time the specs of the runtime of the best block are changed.
    ///
    /// The stream can generate an `Err(())` if the runtime in the best block is invalid.
    pub async fn subscribe_runtime_version(
        self: &Arc<RuntimeService>,
    ) -> (
        Result<executor::CoreVersion, RuntimeError>,
        NotificationsReceiver<Result<executor::CoreVersion, RuntimeError>>,
    ) {
        let (tx, rx) = lossy_channel::channel();
        let mut guarded = self.guarded.lock().await;
        guarded.runtime_version_subscriptions.push(tx);
        let current_version = guarded
            .tree
            .as_ref()
            .unwrap()
            .best_block_runtime()
            .runtime
            .as_ref()
            .map(|spec| spec.runtime_spec.clone())
            .map_err(|err| err.clone());
        (current_version, rx)
    }

    /// Returns the runtime version of the block with the given hash.
    pub async fn runtime_version_of_block(
        self: &Arc<RuntimeService>,
        block_hash: &[u8; 32],
    ) -> Result<executor::CoreVersion, RuntimeCallError> {
        // If the requested block is the best known block, optimize by
        // immediately returning the cached spec.
        {
            let guarded = self.guarded.lock().await;
            if guarded.tree.as_ref().unwrap().best_block_hash() == block_hash {
                return guarded
                    .tree
                    .as_ref()
                    .unwrap()
                    .best_block_runtime()
                    .runtime
                    .as_ref()
                    .map(|r| r.runtime_spec.clone())
                    .map_err(|err| RuntimeCallError::InvalidRuntime(err.clone()));
            }
        }

        let (_, vm) = self.network_block_info(block_hash).await?;

        let (runtime_spec, _) = match executor::core_version(vm) {
            (Ok(spec), vm) => (spec, vm),
            (Err(error), _) => {
                log::warn!(
                    target: &self.log_target,
                    "Failed to call Core_version on runtime: {}",
                    error
                );
                return Err(RuntimeCallError::InvalidRuntime(RuntimeError::CoreVersion(
                    error,
                )));
            }
        };

        Ok(runtime_spec)
    }

    /// Downloads from the network the SCALE-encoded header and the runtime of the block with
    /// the given hash.
    async fn network_block_info(
        self: &Arc<RuntimeService>,
        block_hash: &[u8; 32],
    ) -> Result<(Vec<u8>, executor::host::HostVmPrototype), RuntimeCallError> {
        // Ask the network for the header of this block, as we need to know the state root.
        let header = {
            let result = self
                .sync_service
                .clone()
                .block_query(
                    *block_hash,
                    protocol::BlocksRequestFields {
                        header: true,
                        body: false,
                        justification: false,
                    },
                )
                .await;

            // Note that the `block_query` method guarantees that the header is present
            // and valid.
            if let Ok(block) = result {
                block.header.unwrap()
            } else {
                return Err(RuntimeCallError::NetworkBlockRequest); // TODO: precise error
            }
        };

        let state_root = *header::decode(&header)
            .map_err(RuntimeCallError::InvalidBlockHeader)?
            .state_root;

        // Download the runtime code of this block.
        let (code, heap_pages) = {
            let mut code_query_result = self
                .sync_service
                .clone()
                .storage_query(
                    block_hash,
                    &state_root,
                    iter::once(&b":code"[..]).chain(iter::once(&b":heappages"[..])),
                )
                .await
                .map_err(RuntimeCallError::StorageQuery)?;
            let heap_pages = code_query_result.pop().unwrap();
            let code = code_query_result.pop().unwrap();
            (code, heap_pages)
        };

        let vm = match executor::host::HostVmPrototype::new(
            code.as_ref()
                .ok_or(RuntimeError::CodeNotFound)
                .map_err(RuntimeCallError::InvalidRuntime)?,
            executor::storage_heap_pages_to_value(heap_pages.as_deref())
                .map_err(RuntimeError::InvalidHeapPages)
                .map_err(RuntimeCallError::InvalidRuntime)?,
            executor::vm::ExecHint::CompileAheadOfTime,
        ) {
            Ok(vm) => vm,
            Err(error) => {
                log::warn!(
                    target: &self.log_target,
                    "Failed to compile best block runtime: {}",
                    error
                );
                return Err(RuntimeCallError::InvalidRuntime(RuntimeError::Build(error)));
            }
        };

        Ok((header, vm))
    }

    /// Returns the runtime version of the current best block.
    pub async fn best_block_runtime(
        self: &Arc<RuntimeService>,
    ) -> Result<executor::CoreVersion, RuntimeError> {
        let guarded = self.guarded.lock().await;
        guarded
            .tree
            .as_ref()
            .unwrap()
            .best_block_runtime()
            .runtime
            .as_ref()
            .map(|spec| spec.runtime_spec.clone())
            .map_err(|err| err.clone())
    }

    /// Returns the SCALE-encoded header of the current finalized block, plus an unlimited stream
    /// that produces one item every time the finalized block is changed.
    ///
    /// It is guaranteed that when a notification is sent out, calling
    /// [`RuntimeService::recent_finalized_block_runtime_lock`] will operate on this block or more
    /// recent. In other words, if you call [`RuntimeService::recent_finalized_block_runtime_lock`]
    /// and the stream of notifications is empty, you are guaranteed that the call has been
    /// performed on the finalized block.
    pub async fn subscribe_finalized(
        self: &Arc<RuntimeService>,
    ) -> (Vec<u8>, NotificationsReceiver<Vec<u8>>) {
        let (tx, rx) = lossy_channel::channel();
        let mut guarded = self.guarded.lock().await;
        guarded.finalized_blocks_subscriptions.push(tx);
        (
            guarded
                .tree
                .as_ref()
                .unwrap()
                .finalized_block_header()
                .to_vec(),
            rx,
        )
    }

    /// Returns the SCALE-encoded header of the current best block, plus an unlimited stream that
    /// produces one item every time the best block is changed.
    ///
    /// It is guaranteed that when a notification is sent out, calling
    /// [`RuntimeService::recent_best_block_runtime_lock`] will operate on this block or more
    /// recent. In other words, if you call [`RuntimeService::recent_best_block_runtime_lock`] and
    /// the stream of notifications is empty, you are guaranteed that the call has been performed
    /// on the best block.
    pub async fn subscribe_best(
        self: &Arc<RuntimeService>,
    ) -> (Vec<u8>, NotificationsReceiver<Vec<u8>>) {
        let (tx, rx) = lossy_channel::channel();
        let mut guarded = self.guarded.lock().await;
        guarded.best_blocks_subscriptions.push(tx);
        (
            guarded.tree.as_ref().unwrap().best_block_header().to_vec(),
            rx,
        )
    }

    /// Subscribes to the state of the chain: the current state and the new blocks.
    ///
    /// Contrary to [`RuntimeService::subscribe_best`], *all* new blocks are reported. Only up to
    /// `buffer_size` block notifications are buffered in the channel. If the channel is full
    /// when a new notification is attempted to be pushed, the channel gets closed.
    ///
    /// The channel also gets closed if a gap in the finality happens, such as after a Grandpa
    /// warp syncing.
    ///
    /// See [`sync_service::SubscribeAll`] for information about the return value.
    pub async fn subscribe_all(
        self: &Arc<RuntimeService>,
        buffer_size: usize,
    ) -> sync_service::SubscribeAll {
        let (tx, new_blocks) = mpsc::channel(buffer_size);
        let mut guarded = self.guarded.lock().await;
        debug_assert!(guarded.tree.as_ref().unwrap().has_output());
        guarded.all_blocks_subscriptions.push(tx);

        let tree = guarded.tree.as_ref().unwrap();

        let non_finalized_blocks_ancestry_order: Vec<_> = tree
            .non_finalized_blocks_headers_ancestry_order()
            .map(|(scale_encoded_header, is_new_best)| {
                let parent_hash = *header::decode(scale_encoded_header).unwrap().parent_hash; // TODO: correct? if yes, document
                debug_assert!(
                    parent_hash == *tree.finalized_block_hash()
                        || tree
                            .non_finalized_blocks_headers_ancestry_order()
                            .any(|(h, _)| parent_hash == header::hash_from_scale_encoded_header(h))
                );
                sync_service::BlockNotification {
                    is_new_best,
                    parent_hash,
                    scale_encoded_header: scale_encoded_header.to_vec(),
                }
            })
            .collect();

        debug_assert!(matches!(
            non_finalized_blocks_ancestry_order
                .iter()
                .filter(|b| b.is_new_best)
                .count(),
            0 | 1
        ));

        sync_service::SubscribeAll {
            finalized_block_scale_encoded_header: tree.finalized_block_header().to_vec(),
            new_blocks,
            non_finalized_blocks_ancestry_order,
        }
    }

    // TODO: doc
    pub async fn recent_finalized_block_runtime_lock<'a>(
        self: &'a Arc<RuntimeService>,
    ) -> RuntimeLock<'a> {
        let guarded = self.guarded.lock().await;
        let block_hash = *guarded.tree.as_ref().unwrap().finalized_block_hash();
        RuntimeLock {
            service: self,
            inner: RuntimeLockInner::InTree(guarded),
            block_hash,
        }
    }

    // TODO: doc
    pub async fn recent_best_block_runtime_lock<'a>(
        self: &'a Arc<RuntimeService>,
    ) -> RuntimeLock<'a> {
        let guarded = self.guarded.lock().await;
        let block_hash = *guarded.tree.as_ref().unwrap().best_block_hash();
        RuntimeLock {
            service: self,
            inner: RuntimeLockInner::InTree(guarded),
            block_hash,
        }
    }

    // TODO: should have a LRU cache of slightly older finalized blocks
    // TODO: doc, especially about which blocks are available
    // TODO: return error instead
    pub async fn runtime_lock<'a>(
        self: &'a Arc<RuntimeService>,
        block_hash: &[u8; 32],
    ) -> Option<RuntimeLock<'a>> {
        let guarded = self.guarded.lock().await;
        if guarded
            .tree
            .as_ref()
            .unwrap()
            .block_runtime(block_hash)
            .is_some()
        {
            return Some(RuntimeLock {
                service: self,
                inner: RuntimeLockInner::InTree(guarded),
                block_hash: *block_hash,
            });
        }

        let (scale_encoded_header, virtual_machine) =
            self.network_block_info(block_hash).await.ok()?;
        Some(RuntimeLock {
            service: self,
            inner: RuntimeLockInner::OutOfTree {
                scale_encoded_header,
                virtual_machine,
            },
            block_hash: *block_hash,
        })
    }

    /// Obtain the metadata of the runtime of the current best block.
    ///
    /// > **Note**: Keep in mind that this function is subject to race conditions. The runtime
    /// >           of the best block can change at any time. This method should ideally be called
    /// >           again after every runtime change.
    pub async fn metadata(self: Arc<RuntimeService>) -> Result<Vec<u8>, MetadataError> {
        // First, try the cache.
        {
            let guarded = self.guarded.lock().await;
            match guarded
                .tree
                .as_ref()
                .unwrap()
                .best_block_runtime()
                .runtime
                .as_ref()
            {
                Ok(runtime) => {
                    if let Some(metadata) = runtime.metadata.as_ref() {
                        return Ok(metadata.clone());
                    }
                }
                Err(err) => {
                    return Err(MetadataError::InvalidRuntime(err.clone()));
                }
            }
        }

        let (mut runtime_call_lock, virtual_machine) = self
            .recent_best_block_runtime_lock()
            .await
            .start("Metadata_metadata", iter::empty::<Vec<u8>>())
            .await
            .map_err(MetadataError::CallError)?;

        let mut query = metadata::query_metadata(virtual_machine);
        let (metadata_result, virtual_machine) = loop {
            match query {
                metadata::Query::Finished(Ok(metadata), virtual_machine) => {
                    if let Some(guarded) = &mut runtime_call_lock.guarded {
                        guarded
                            .tree
                            .as_mut()
                            .unwrap()
                            .best_block_runtime_mut()
                            .runtime
                            .as_mut()
                            .unwrap()
                            .metadata = Some(metadata.clone());
                    }
                    break (Ok(metadata), virtual_machine);
                }
                metadata::Query::StorageGet(storage_get) => {
                    match runtime_call_lock.storage_entry(&storage_get.key_as_vec()) {
                        Ok(v) => query = storage_get.inject_value(v.map(iter::once)),
                        Err(err) => {
                            break (
                                Err(MetadataError::CallError(err)),
                                metadata::Query::StorageGet(storage_get).into_prototype(),
                            );
                        }
                    }
                }
                metadata::Query::Finished(Err(err), virtual_machine) => {
                    break (Err(MetadataError::MetadataQuery(err)), virtual_machine);
                }
            }
        };

        runtime_call_lock.unlock(virtual_machine);
        metadata_result
    }

    /// Returns true if it is believed that we are near the head of the chain.
    ///
    /// The way this method is implemented is opaque and cannot be relied on. The return value
    /// should only ever be shown to the user and not used for any meaningful logic.
    pub async fn is_near_head_of_chain_heuristic(&self) -> bool {
        // The runtime service adds a delay between the moment a best block is reported by the
        // sync service and the moment it is reported by the runtime service.
        // Because of this, any "far from head of chain" to "near head of chain" transition
        // must take that delay into account. The other way around ("near" to "far") is
        // unaffected.

        // If the sync service is far from the head, the runtime service is also far.
        if !self.sync_service.is_near_head_of_chain_heuristic().await {
            return false;
        }

        // If the sync service is near, report the result of `is_near_head_of_chain_heuristic()`
        // when called at the latest best block that the runtime service reported through its API,
        // to make sure that we don't report "near" while having reported only blocks that were
        // far.
        self.guarded.lock().await.best_near_head_of_chain
    }
}

/// See [`RuntimeService::recent_best_block_runtime_lock`].
#[must_use]
pub struct RuntimeLock<'a> {
    service: &'a Arc<RuntimeService>,
    inner: RuntimeLockInner<'a>,
    /// Hash of the block to make the call against.
    block_hash: [u8; 32],
}

enum RuntimeLockInner<'a> {
    /// Block is found in [`Guarded::tree`].
    InTree(MutexGuard<'a, Guarded>),
    /// Block information directly inlined in this enum.
    OutOfTree {
        scale_encoded_header: Vec<u8>,
        virtual_machine: executor::host::HostVmPrototype,
    },
}

impl<'a> RuntimeLock<'a> {
    /// Returns the SCALE-encoded header of the block the call is being made against.
    ///
    /// Guaranteed to always be valid.
    pub fn block_scale_encoded_header(&self) -> &[u8] {
        match &self.inner {
            RuntimeLockInner::InTree(guarded) => guarded
                .tree
                .as_ref()
                .unwrap()
                .block_header(&self.block_hash)
                .unwrap(),
            RuntimeLockInner::OutOfTree {
                scale_encoded_header,
                ..
            } => scale_encoded_header,
        }
    }

    /// Returns the hash of the block the call is being made against.
    pub fn block_hash(&self) -> &[u8; 32] {
        &self.block_hash
    }

    pub fn runtime(&self) -> &executor::host::HostVmPrototype {
        match &self.inner {
            RuntimeLockInner::InTree(guarded) => {
                let tree = guarded.tree.as_ref().unwrap();
                tree.block_runtime(&self.block_hash)
                    .unwrap()
                    .runtime
                    .as_ref()
                    .unwrap()
                    .virtual_machine
                    .as_ref()
                    .unwrap()
            }
            RuntimeLockInner::OutOfTree {
                virtual_machine, ..
            } => virtual_machine,
        }
    }

    pub async fn start<'b>(
        self,
        method: &'b str,
        parameter_vectored: impl Iterator<Item = impl AsRef<[u8]>> + Clone + 'b,
    ) -> Result<(RuntimeCallLock<'a>, executor::host::HostVmPrototype), RuntimeCallError> {
        // TODO: DRY :-/ this whole thing is messy

        let block_number = header::decode(&self.block_scale_encoded_header())
            .unwrap()
            .number;
        let block_hash = *self.block_hash();
        let runtime_block_header = self.block_scale_encoded_header().to_owned(); // TODO: cloning :-/
        let virtual_machine = match self.inner {
            RuntimeLockInner::InTree(lock) => {
                // Unlock `guarded` before doing anything that takes a long time, such as the
                // network request below.
                drop(lock);
                None
            }
            RuntimeLockInner::OutOfTree {
                virtual_machine, ..
            } => Some(virtual_machine),
        };

        // Perform the call proof request.
        // Note that `guarded` is not locked.
        // TODO: there's no way to verify that the call proof is actually correct; we have to ban the peer and restart the whole call process if it turns out that it's not
        // TODO: also, an empty proof will be reported as an error right now, which is weird
        let call_proof = self
            .service
            .sync_service
            .clone()
            .call_proof_query(
                block_number,
                protocol::CallProofRequestConfig {
                    block_hash,
                    method,
                    parameter_vectored: parameter_vectored.clone(),
                },
            )
            .await
            .map_err(RuntimeCallError::CallProof);

        let (guarded, virtual_machine) = if let Some(virtual_machine) = virtual_machine {
            (None, virtual_machine)
        } else {
            // Lock `guarded` again now that the call is finished.
            let mut guarded = self.service.guarded.lock().await;

            // It is not guaranteed that the block is still in the tree after the storage proof
            // has ended.
            match guarded
                .tree
                .as_mut()
                .unwrap()
                .block_runtime_mut(&self.block_hash)
            {
                Some(block) => {
                    let virtual_machine = match block.runtime.as_mut() {
                        Ok(r) => r.virtual_machine.take().unwrap(),
                        Err(err) => {
                            return Err(RuntimeCallError::InvalidRuntime(err.clone()));
                        }
                    };

                    (Some(guarded), virtual_machine)
                }
                None => {
                    let (_, virtual_machine) =
                        self.service.network_block_info(&self.block_hash).await?;
                    (None, virtual_machine)
                }
            }
        };

        let lock = RuntimeCallLock {
            guarded,
            block_hash: self.block_hash,
            runtime_block_header,
            call_proof,
        };

        Ok((lock, virtual_machine))
    }
}

/// See [`RuntimeService::recent_best_block_runtime_lock`].
#[must_use]
pub struct RuntimeCallLock<'a> {
    /// If `Some`, the virtual machine must be put back in the tree.
    guarded: Option<MutexGuard<'a, Guarded>>,
    runtime_block_header: Vec<u8>,
    block_hash: [u8; 32],
    call_proof: Result<Vec<Vec<u8>>, RuntimeCallError>,
}

impl<'a> RuntimeCallLock<'a> {
    /// Returns the SCALE-encoded header of the block the call is being made against.
    pub fn block_scale_encoded_header(&self) -> &[u8] {
        &self.runtime_block_header
    }

    /// Returns the storage root of the block the call is being made against.
    pub fn block_storage_root(&self) -> &[u8; 32] {
        header::decode(&self.runtime_block_header)
            .unwrap()
            .state_root
    }

    /// Finds the given key in the call proof and returns the associated storage value.
    ///
    /// Returns an error if the key couldn't be found in the proof, meaning that the proof is
    /// invalid.
    // TODO: if proof is invalid, we should give the option to fetch another call proof
    pub fn storage_entry(&self, requested_key: &[u8]) -> Result<Option<&[u8]>, RuntimeCallError> {
        let call_proof = match &self.call_proof {
            Ok(p) => p,
            Err(err) => return Err(err.clone()),
        };

        match proof_verify::verify_proof(proof_verify::VerifyProofConfig {
            requested_key: &requested_key,
            trie_root_hash: self.block_storage_root(),
            proof: call_proof.iter().map(|v| &v[..]),
        }) {
            Ok(v) => Ok(v),
            Err(err) => Err(RuntimeCallError::StorageRetrieval(err)),
        }
    }

    /// Finds in the call proof the list of keys that match a certain prefix.
    ///
    /// Returns an error if not all the keys could be found in the proof, meaning that the proof
    /// is invalid.
    ///
    /// The keys returned are ordered lexicographically.
    // TODO: if proof is invalid, we should give the option to fetch another call proof
    pub fn storage_prefix_keys_ordered(
        &'_ self,
        prefix: &[u8],
    ) -> Result<impl Iterator<Item = impl AsRef<[u8]> + '_>, RuntimeCallError> {
        // TODO: this is sub-optimal as we iterate over the proof multiple times and do a lot of Vec allocations
        let mut to_find = vec![trie::bytes_to_nibbles(prefix.iter().copied()).collect::<Vec<_>>()];
        let mut output = Vec::new();

        let call_proof = match &self.call_proof {
            Ok(p) => p,
            Err(err) => return Err(err.clone()),
        };

        for key in mem::replace(&mut to_find, Vec::new()) {
            let node_info = proof_verify::trie_node_info(proof_verify::TrieNodeInfoConfig {
                requested_key: key.iter().cloned(),
                trie_root_hash: &self.block_storage_root(),
                proof: call_proof.iter().map(|v| &v[..]),
            })
            .map_err(RuntimeCallError::StorageRetrieval)?;

            if node_info.storage_value.is_some() {
                assert_eq!(key.len() % 2, 0);
                output.push(trie::nibbles_to_bytes_extend(key.iter().copied()).collect::<Vec<_>>());
            }

            match node_info.children {
                proof_verify::Children::None => {}
                proof_verify::Children::One(nibble) => {
                    let mut child = key.clone();
                    child.push(nibble);
                    to_find.push(child);
                }
                proof_verify::Children::Multiple { children_bitmap } => {
                    for nibble in trie::all_nibbles() {
                        if (children_bitmap & (1 << u8::from(nibble))) == 0 {
                            continue;
                        }

                        let mut child = key.clone();
                        child.push(nibble);
                        to_find.push(child);
                    }
                }
            }
        }

        // TODO: maybe we could iterate over the proof in an ordered way rather than sorting at the end
        output.sort();
        Ok(output.into_iter())
    }

    /// End the runtime call.
    ///
    /// This method **must** be called.
    pub fn unlock(mut self, vm: executor::host::HostVmPrototype) {
        if let Some(guarded) = &mut self.guarded {
            guarded
                .tree
                .as_mut()
                .unwrap()
                .block_runtime_mut(&self.block_hash)
                .unwrap()
                .runtime
                .as_mut()
                .unwrap()
                .virtual_machine = Some(vm);
        }
    }
}

impl<'a> Drop for RuntimeCallLock<'a> {
    fn drop(&mut self) {
        if let Some(guarded) = &mut self.guarded {
            let vm = &mut guarded
                .tree
                .as_mut()
                .unwrap()
                .block_runtime_mut(&self.block_hash)
                .unwrap()
                .runtime
                .as_mut()
                .unwrap()
                .virtual_machine;

            if vm.is_none() {
                // The [`RuntimeCallLock`] has been destroyed without being properly unlocked.
                panic!()
            }
        }
    }
}

/// Error that can happen when calling a runtime function.
#[derive(Debug, Clone, derive_more::Display)]
pub enum RuntimeCallError {
    /// Runtime of the best block isn't valid.
    #[display(fmt = "Runtime of the best block isn't valid: {}", _0)]
    InvalidRuntime(RuntimeError),
    /// Error while retrieving the storage item from other nodes.
    // TODO: change error type?
    #[display(fmt = "Error in call proof: {}", _0)]
    StorageRetrieval(proof_verify::Error),
    /// Error while retrieving the call proof from the network.
    #[display(fmt = "Error when retrieving the call proof: {}", _0)]
    CallProof(sync_service::CallProofQueryError),
    /// Error while performing the block request on the network.
    NetworkBlockRequest, // TODO: precise error
    /// Failed to decode the header of the block.
    #[display(fmt = "Failed to decode header of the block: {}", _0)]
    InvalidBlockHeader(header::Error),
    /// Error while querying the storage of the block.
    #[display(fmt = "Error while querying block storage: {}", _0)]
    StorageQuery(sync_service::StorageQueryError),
}

impl RuntimeCallError {
    /// Returns `true` if this is caused by networking issues, as opposed to a consensus-related
    /// issue.
    pub fn is_network_problem(&self) -> bool {
        match self {
            RuntimeCallError::InvalidRuntime(_) => false,
            // TODO: as a temporary hack, we consider `TrieRootNotFound` as the remote not knowing about the requested block; see https://github.com/paritytech/substrate/pull/8046
            RuntimeCallError::StorageRetrieval(proof_verify::Error::TrieRootNotFound) => true,
            RuntimeCallError::StorageRetrieval(_) => false,
            RuntimeCallError::CallProof(err) => err.is_network_problem(),
            RuntimeCallError::InvalidBlockHeader(_) => false,
            RuntimeCallError::NetworkBlockRequest => true,
            RuntimeCallError::StorageQuery(err) => err.is_network_problem(),
        }
    }
}

/// Error that can happen when calling [`RuntimeService::metadata`].
#[derive(Debug, derive_more::Display)]
pub enum MetadataError {
    /// Error during the runtime call.
    #[display(fmt = "{}", _0)]
    CallError(RuntimeCallError),
    /// Runtime of the best block isn't valid.
    #[display(fmt = "Runtime of the best block isn't valid: {}", _0)]
    InvalidRuntime(RuntimeError),
    /// Error in the metadata-specific runtime API.
    #[display(fmt = "Error in the metadata-specific runtime API: {}", _0)]
    MetadataQuery(metadata::Error),
}

struct Guarded {
    /// List of senders that get notified when the runtime specs of the best block changes.
    /// Whenever the best block runtime is updated, one should emit an item on each sender.
    /// See [`RuntimeService::subscribe_runtime_version`].
    runtime_version_subscriptions:
        Vec<lossy_channel::Sender<Result<executor::CoreVersion, RuntimeError>>>,

    /// List of senders that get notified when new blocks arrive.
    /// See [`RuntimeService::subscribe_all`].
    all_blocks_subscriptions: Vec<mpsc::Sender<sync_service::Notification>>,

    /// List of senders that get notified when the finalized block is updated.
    /// See [`RuntimeService::subscribe_finalized`].
    finalized_blocks_subscriptions: Vec<lossy_channel::Sender<Vec<u8>>>,

    /// List of senders that get notified when the best block is updated.
    /// See [`RuntimeService::subscribe_best`].
    best_blocks_subscriptions: Vec<lossy_channel::Sender<Vec<u8>>>,

    /// Return value of calling [`sync_service::SyncService::is_near_head_of_chain_heuristic`]
    /// after the latest best block update.
    best_near_head_of_chain: bool,

    /// Tree of blocks. Holds the state of the download of everything. Always `true` when the
    /// `Mutex` is being locked. Switched to `None` during some operations.
    tree: Option<download_tree::DownloadTree<ffi::Instant, Runtime>>,
}

impl Guarded {
    /// Notifies the subscribers about changes to the best and finalized blocks.
    fn notify_subscribers(
        &mut self,
        best_block_updated: bool,
        best_block_runtime_changed: bool,
        finalized_block_updated: bool,
    ) {
        if best_block_updated {
            let best_block_header = self.tree.as_ref().unwrap().best_block_header();

            // Elements are removed one by one and inserted back if the channel is still open.
            for index in (0..self.best_blocks_subscriptions.len()).rev() {
                let mut subscription = self.best_blocks_subscriptions.swap_remove(index);
                if subscription.send(best_block_header.to_vec()).is_err() {
                    continue;
                }

                self.best_blocks_subscriptions.push(subscription);
            }
        }

        if finalized_block_updated {
            let finalized_block_header = self.tree.as_ref().unwrap().finalized_block_header();

            // Elements are removed one by one and inserted back if the channel is still open.
            for index in (0..self.finalized_blocks_subscriptions.len()).rev() {
                let mut subscription = self.finalized_blocks_subscriptions.swap_remove(index);
                if subscription.send(finalized_block_header.to_vec()).is_err() {
                    continue;
                }

                self.finalized_blocks_subscriptions.push(subscription);
            }
        }

        if best_block_runtime_changed {
            let runtime_version = &self
                .tree
                .as_ref()
                .unwrap()
                .best_block_runtime()
                .runtime
                .as_ref();

            // Elements are removed one by one and inserted back if the channel is still open.
            for index in (0..self.runtime_version_subscriptions.len()).rev() {
                let mut subscription = self.runtime_version_subscriptions.swap_remove(index);
                if subscription
                    .send(
                        runtime_version
                            .map(|v| v.runtime_spec.clone())
                            .map_err(|e| e.clone()),
                    )
                    .is_err()
                {
                    continue;
                }

                self.runtime_version_subscriptions.push(subscription);
            }
        }
    }
}

async fn run_background(original_runtime_service: Arc<RuntimeService>) {
    loop {
        // The buffer size should be large enough so that, if the CPU is busy, it doesn't
        // become full before the execution of the runtime service resumes.
        let subscription = original_runtime_service
            .sync_service
            .subscribe_all(16)
            .await;

        log::debug!(
            target: &original_runtime_service.log_target,
            "Reinitialized background worker to finalized block {}",
            HashDisplay(&header::hash_from_scale_encoded_header(&subscription.finalized_block_scale_encoded_header))
            // TODO: print block height
        );

        // In order to bootstrap the new runtime service, a fresh temporary runtime service is
        // created.
        // Later, when the `Guarded` contains at least a finalized runtime, it will be written
        // over the original runtime service.
        // TODO: if subscription.finalized is equal to current finalized, skip the whole process below?
        let mut background = Background {
            runtime_service: Arc::new(RuntimeService {
                log_target: original_runtime_service.log_target.clone(),
                sync_service: original_runtime_service.sync_service.clone(),
                guarded: Mutex::new(Guarded {
                    all_blocks_subscriptions: Vec::new(),
                    best_blocks_subscriptions: Vec::new(),
                    finalized_blocks_subscriptions: Vec::new(),
                    runtime_version_subscriptions: Vec::new(),
                    best_near_head_of_chain: original_runtime_service
                        .is_near_head_of_chain_heuristic()
                        .await,
                    tree: Some(download_tree::DownloadTree::from_finalized_block(
                        subscription.finalized_block_scale_encoded_header,
                    )),
                }),
            }),
            blocks_stream: subscription.new_blocks.boxed(),
            wake_up_new_necessary_download: future::pending().boxed().fuse(),
            runtime_downloads: stream::FuturesUnordered::new(),
        };

        for block in subscription.non_finalized_blocks_ancestry_order {
            let _ = background
                .runtime_service
                .guarded
                .try_lock()
                .unwrap()
                .tree
                .as_mut()
                .unwrap()
                .input_insert_block(
                    block.scale_encoded_header,
                    &block.parent_hash,
                    block.is_new_best,
                );
        }

        background.start_necessary_downloads().await;

        // Inner loop. Process incoming events.
        loop {
            if !Arc::ptr_eq(&background.runtime_service, &original_runtime_service) {
                // The `Background` object is manipulating a temporary runtime service. Check if
                // it is possible to write to the original runtime service.
                let mut temporary_guarded = background.runtime_service.guarded.try_lock().unwrap();
                if temporary_guarded.tree.as_ref().unwrap().has_output() {
                    log::debug!(
                        target: &original_runtime_service.log_target,
                        "Background worker now in sync"
                    );

                    let mut original_guarded = original_runtime_service.guarded.lock().await;
                    original_guarded.best_near_head_of_chain =
                        temporary_guarded.best_near_head_of_chain;
                    original_guarded.tree = Some(temporary_guarded.tree.take().unwrap());

                    drop(temporary_guarded);

                    original_guarded.all_blocks_subscriptions.clear();
                    // TODO: correct? especially for the runtime?
                    original_guarded.notify_subscribers(true, true, true);

                    background.runtime_service = original_runtime_service.clone();
                }
            }

            futures::select! {
                _ = &mut background.wake_up_new_necessary_download => {
                    background.start_necessary_downloads().await;
                },
                notification = background.blocks_stream.next().fuse() => {
                    match notification {
                        None => break, // Break out of the inner loop in order to reset the background.
                        Some(sync_service::Notification::Block(new_block)) => {
                            log::debug!(
                                target: &original_runtime_service.log_target,
                                "New sync service block: hash={}, parent={}, is_new_best={}",
                                HashDisplay(&header::hash_from_scale_encoded_header(&new_block.scale_encoded_header)),
                                HashDisplay(&new_block.parent_hash),
                                new_block.is_new_best
                            );

                            let near_head_of_chain = background.runtime_service.sync_service.is_near_head_of_chain_heuristic().await;

                            let mut guarded = background.runtime_service.guarded.lock().await;
                            // TODO: note that this code is never reached for parachains
                            if new_block.is_new_best {
                                guarded.best_near_head_of_chain = near_head_of_chain;
                            }
                            guarded.tree.as_mut().unwrap().input_insert_block(new_block.scale_encoded_header, &new_block.parent_hash, new_block.is_new_best);
                            background.advance_and_notify_subscribers(&mut guarded);
                        },
                        Some(sync_service::Notification::Finalized { hash, best_block_hash }) => {
                            log::debug!(
                                target: &original_runtime_service.log_target,
                                "New sync service finalization: hash={}, new_best={}",
                                HashDisplay(&hash), HashDisplay(&best_block_hash)
                            );

                            background.finalize(hash, best_block_hash).await;
                        }
                    };

                    // TODO: process any other pending event from blocks_stream before doing that; otherwise we might start download for blocks that we don't care about because they're immediately overwritten by others
                    background.start_necessary_downloads().await;
                },
                (download_id, download_result) = background.runtime_downloads.select_next_some() => {
                    match download_result {
                        Ok((storage_code, storage_heap_pages)) => {
                            log::debug!(
                                target: &original_runtime_service.log_target,
                                "Successfully finished download of id {:?}",
                                download_id
                            );

                            // TODO: the line below is a complete hack; the code that updates this value is never reached for parachains, and as such the line below is here to update this field
                            background.runtime_service.guarded.lock().await.best_near_head_of_chain = true;

                            background.runtime_download_finished(download_id, storage_code, storage_heap_pages).await;
                        }
                        Err(error) => {
                            log::log!(
                                target: &original_runtime_service.log_target,
                                if error.is_network_problem() {
                                    log::Level::Debug
                                } else {
                                    log::Level::Warn
                                },
                                // TODO: better message
                                "Failed to download :code and :heappages of block: {}",
                                error
                            );

                            let mut guarded = background.runtime_service.guarded.lock().await;
                            guarded.tree.as_mut().unwrap().runtime_download_failure(download_id, &ffi::Instant::now());
                        }
                    }

                    background.start_necessary_downloads().await;
                }
            }
        }
    }
}

struct Background {
    runtime_service: Arc<RuntimeService>,

    /// Stream of blocks updates coming from the sync service.
    /// Initially has a dummy value.
    blocks_stream: Pin<Box<dyn Stream<Item = sync_service::Notification> + Send>>,

    /// List of runtimes currently being downloaded from the network.
    /// For each item, the download id, storage value of `:code`, and storage value of
    /// `:heappages`.
    runtime_downloads: stream::FuturesUnordered<
        future::BoxFuture<
            'static,
            (
                download_tree::DownloadId,
                Result<(Option<Vec<u8>>, Option<Vec<u8>>), StorageQueryError>,
            ),
        >,
    >,

    /// Future that wakes up when a new download to start is potentially ready.
    wake_up_new_necessary_download: future::Fuse<future::BoxFuture<'static, ()>>,
}

impl Background {
    /// Injects into the state of `self` a completed runtime download.
    async fn runtime_download_finished(
        &mut self,
        download_id: download_tree::DownloadId,
        storage_code: Option<Vec<u8>>,
        storage_heap_pages: Option<Vec<u8>>,
    ) {
        let mut guarded = self.runtime_service.guarded.lock().await;

        let existing_runtime = guarded
            .tree
            .as_ref()
            .unwrap()
            .runtimes_iter()
            .find(|(_, rt)| rt.runtime_code == storage_code && rt.heap_pages == storage_heap_pages)
            .map(|(id, _)| id);

        if let Some(existing_runtime) = existing_runtime {
            guarded
                .tree
                .as_mut()
                .unwrap()
                .runtime_download_finished_existing(download_id, existing_runtime)
        } else {
            let runtime = SuccessfulRuntime::from_params(&storage_code, &storage_heap_pages).await;

            guarded
                .tree
                .as_mut()
                .unwrap()
                .runtime_download_finished_new(
                    download_id,
                    Runtime {
                        heap_pages: storage_heap_pages,
                        runtime_code: storage_code,
                        runtime,
                    },
                );
        }

        self.advance_and_notify_subscribers(&mut guarded);
    }

    fn advance_and_notify_subscribers(&self, guarded: &mut Guarded) {
        let tree = guarded.tree.as_mut().unwrap();

        let mut best_block_updated = false;
        let mut best_block_runtime_changed = false;
        let mut finalized_block_updated = false;

        loop {
            let notif = match tree.try_advance_output() {
                None | Some(download_tree::OutputUpdate::None) => break,
                Some(notif) => notif,
            };

            let all_blocks_notif = match notif {
                download_tree::OutputUpdate::None => unreachable!(),
                download_tree::OutputUpdate::FirstFinalized { .. } => {
                    best_block_updated = true;
                    finalized_block_updated = true;
                    best_block_runtime_changed = true; // TODO: ?!
                    continue;
                }
                download_tree::OutputUpdate::Finalized {
                    hash,
                    best_block_hash,
                } => {
                    best_block_updated = true;
                    finalized_block_updated = true;
                    best_block_runtime_changed = true; // TODO: ?!

                    sync_service::Notification::Finalized {
                        best_block_hash: *best_block_hash,
                        hash: *hash,
                    }
                }
                download_tree::OutputUpdate::Block(download_tree::OutputUpdateBlock {
                    is_new_best:
                        is_new_best @ download_tree::OutputUpdateBlockBest::NewBest
                        | is_new_best @ download_tree::OutputUpdateBlockBest::NewBestAndRuntimeUpgrade,
                    parent_hash,
                    scale_encoded_header,
                }) => {
                    best_block_updated = true;
                    if let download_tree::OutputUpdateBlockBest::NewBestAndRuntimeUpgrade =
                        is_new_best
                    {
                        best_block_runtime_changed = true;
                    }

                    sync_service::Notification::Block(sync_service::BlockNotification {
                        parent_hash: *parent_hash,
                        is_new_best: true,
                        scale_encoded_header: scale_encoded_header.to_vec(),
                    })
                }
                download_tree::OutputUpdate::Block(block) => {
                    sync_service::Notification::Block(sync_service::BlockNotification {
                        parent_hash: *block.parent_hash,
                        is_new_best: false,
                        scale_encoded_header: block.scale_encoded_header.to_vec(),
                    })
                }
            };

            // Elements are removed one by one and inserted back if the channel is still open.
            for index in (0..guarded.all_blocks_subscriptions.len()).rev() {
                let mut subscription = guarded.all_blocks_subscriptions.swap_remove(index);
                if subscription.try_send(all_blocks_notif.clone()).is_err() {
                    continue;
                }

                guarded.all_blocks_subscriptions.push(subscription);
            }
        }

        guarded.notify_subscribers(
            best_block_updated,
            best_block_runtime_changed,
            finalized_block_updated,
        );
    }

    /// Examines the state of `self` and starts downloading runtimes if necessary.
    async fn start_necessary_downloads(&mut self) {
        let mut guarded = self.runtime_service.guarded.lock().await;
        let guarded = &mut *guarded;

        loop {
            // Don't download more than 2 runtimes at a time.
            if self.runtime_downloads.len() >= 2 {
                break;
            }

            // If there's nothing more to download, break out of the loop.
            let download_params = match guarded
                .tree
                .as_mut()
                .unwrap()
                .next_necessary_download(&ffi::Instant::now())
            {
                download_tree::NextNecessaryDownload::Ready(dl) => dl,
                download_tree::NextNecessaryDownload::NotReady { when } => {
                    self.wake_up_new_necessary_download = if let Some(when) = when {
                        ffi::Delay::new_at(when).boxed()
                    } else {
                        future::pending().boxed()
                    }
                    .fuse();
                    break;
                }
            };

            log::debug!(
                target: &self.runtime_service.log_target,
                "Starting new download, id={:?}, block={}",
                download_params.id,
                HashDisplay(&download_params.block_hash)
            );

            // Dispatches a runtime download task to `runtime_downloads`.
            self.runtime_downloads.push(Box::pin({
                let sync_service = self.runtime_service.sync_service.clone();

                async move {
                    let result = sync_service
                        .storage_query(
                            &download_params.block_hash,
                            &download_params.block_state_root,
                            iter::once(&b":code"[..]).chain(iter::once(&b":heappages"[..])),
                        )
                        .await;

                    let result = match result {
                        Ok(mut c) => {
                            let heap_pages = c.pop().unwrap();
                            let code = c.pop().unwrap();
                            Ok((code, heap_pages))
                        }
                        Err(error) => Err(error),
                    };

                    (download_params.id, result)
                }
            }));
        }
    }

    /// Updates `self` to take into account that the sync service has finalized the given block.
    async fn finalize(&mut self, hash_to_finalize: [u8; 32], new_best_block_hash: [u8; 32]) {
        let mut guarded = self.runtime_service.guarded.lock().await;

        guarded
            .tree
            .as_mut()
            .unwrap()
            .input_finalize(hash_to_finalize, new_best_block_hash);

        self.advance_and_notify_subscribers(&mut guarded);

        // Clean up unused runtimes to free up resources.
        for _ in guarded.tree.as_mut().unwrap().drain_unused_runtimes() {}
    }
}

struct Runtime {
    /// Successfully-compiled runtime and all its information. Can contain an error if an error
    /// happened, including a problem when obtaining the runtime specs.
    runtime: Result<SuccessfulRuntime, RuntimeError>,

    /// Undecoded storage value of `:code` corresponding to the [`Runtime::runtime`]
    /// field.
    ///
    /// Can be `None` if the storage is empty, in which case the runtime will have failed to
    /// build.
    // TODO: consider storing hash instead
    runtime_code: Option<Vec<u8>>,

    /// Undecoded storage value of `:heappages` corresponding to the
    /// [`Runtime::runtime`] field.
    ///
    /// Can be `None` if the storage is empty, in which case the runtime will have failed to
    /// build.
    // TODO: consider storing hash instead
    heap_pages: Option<Vec<u8>>,
}

struct SuccessfulRuntime {
    /// Cache of the metadata extracted from the runtime. `None` if unknown.
    ///
    /// This cache is filled lazily whenever it is requested through the public API.
    ///
    /// Note that building the metadata might require access to the storage, just like obtaining
    /// the runtime code. if the runtime code gets an update, we can reasonably assume that the
    /// network is able to serve us the storage of recent blocks, and thus the changes of being
    /// able to build the metadata are very high.
    ///
    /// If the runtime is the one found in the genesis storage, the metadata must have been been
    /// filled using the genesis storage as well. If we build the metadata of the genesis runtime
    /// lazily, chances are that the network wouldn't be able to serve the storage of blocks near
    /// the genesis.
    ///
    /// As documented in the smoldot metadata module, the metadata might access the storage, but
    /// we intentionally don't watch for changes in these storage keys to refresh the metadata.
    metadata: Option<Vec<u8>>,

    /// Runtime specs extracted from the runtime.
    runtime_spec: executor::CoreVersion,

    /// Virtual machine itself, to perform additional calls.
    ///
    /// Always `Some`, except for temporary extractions necessary to execute the VM.
    virtual_machine: Option<executor::host::HostVmPrototype>,
}

impl SuccessfulRuntime {
    async fn from_params(
        code: &Option<Vec<u8>>,
        heap_pages: &Option<Vec<u8>>,
    ) -> Result<Self, RuntimeError> {
        // Since compiling the runtime is a CPU-intensive operation, we yield once before and
        // once after.
        super::yield_once().await;

        let vm = match executor::host::HostVmPrototype::new(
            code.as_ref().ok_or(RuntimeError::CodeNotFound)?,
            executor::storage_heap_pages_to_value(heap_pages.as_deref())
                .map_err(RuntimeError::InvalidHeapPages)?,
            executor::vm::ExecHint::CompileAheadOfTime,
        ) {
            Ok(vm) => vm,
            Err(error) => {
                return Err(RuntimeError::Build(error));
            }
        };

        // Since compiling the runtime is a CPU-intensive operation, we yield once before and
        // once after.
        super::yield_once().await;

        let (runtime_spec, vm) = match executor::core_version(vm) {
            (Ok(spec), vm) => (spec, vm),
            (Err(error), _) => {
                return Err(RuntimeError::CoreVersion(error));
            }
        };

        Ok(SuccessfulRuntime {
            metadata: None,
            runtime_spec,
            virtual_machine: Some(vm),
        })
    }
}
