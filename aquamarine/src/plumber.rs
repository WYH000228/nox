/*
 * Copyright 2020 Fluence Labs Limited
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use eyre::eyre;
use fluence_keypair::KeyPair;
use futures::future::BoxFuture;
use futures::FutureExt;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::task::Poll::Ready;
use std::{
    collections::{HashMap, VecDeque},
    task::{Context, Poll},
};

use futures::task::Waker;
use marine_wasmtime_backend::WasmtimeWasmBackend;
use tokio::runtime::Handle;
use tokio::task;
use tracing::instrument;

use fluence_libp2p::PeerId;
/// For tests, mocked time is used
#[cfg(test)]
use mock_time::now_ms;
use particle_execution::{ParticleFunctionStatic, ParticleParams, ServiceFunction};
use particle_protocol::ExtendedParticle;
use particle_services::PeerScope;
use peer_metrics::{ParticleExecutorMetrics, WorkerLabel, WorkerType};
/// Get current time from OS
#[cfg(not(test))]
use real_time::now_ms;
use types::DealId;
use workers::{KeyStorage, PeerScopes, Workers};

use crate::actor::{Actor, ActorPoll};
use crate::deadline::Deadline;
use crate::error::AquamarineApiError;
use crate::particle_effects::LocalRoutingEffects;
use crate::particle_functions::{Functions, SingleCallStat};
use crate::spawner::{RootSpawner, Spawner, WorkerSpawner};
use crate::vm_pool::VmPool;
use crate::{AquaRuntime, ParticleDataStore, RemoteRoutingEffects};
use types::peer_scope::WorkerId;

#[derive(PartialEq, Hash, Eq)]
struct ActorKey {
    signature: Vec<u8>,
}

const MAX_CLEANUP_KEYS_SIZE: usize = 1024;

pub struct Plumber<RT: AquaRuntime, F> {
    config: RT::Config,
    events: VecDeque<Result<RemoteRoutingEffects, AquamarineApiError>>,
    host_actors: HashMap<ActorKey, Actor<RT, F>>,
    host_vm_pool: VmPool<RT>,
    worker_actors: HashMap<WorkerId, HashMap<ActorKey, Actor<RT, F>>>,
    worker_vm_pools: HashMap<WorkerId, VmPool<RT>>,
    workers: Arc<Workers>,
    data_store: Arc<ParticleDataStore>,
    builtins: F,
    waker: Option<Waker>,
    metrics: Option<ParticleExecutorMetrics>,
    key_storage: Arc<KeyStorage>,
    scopes: PeerScopes,
    cleanup_future: Option<BoxFuture<'static, ()>>,
    root_runtime_handle: Handle,
    avm_wasm_backend: WasmtimeWasmBackend,
}

impl<RT: AquaRuntime, F: ParticleFunctionStatic> Plumber<RT, F> {
    pub fn new(
        config: RT::Config,
        host_vm_pool: VmPool<RT>,
        data_store: Arc<ParticleDataStore>,
        builtins: F,
        metrics: Option<ParticleExecutorMetrics>,
        workers: Arc<Workers>,
        key_storage: Arc<KeyStorage>,
        scope: PeerScopes,
        avm_wasm_backend: WasmtimeWasmBackend,
    ) -> Self {
        Self {
            config,
            host_vm_pool,
            data_store,
            builtins,
            events: <_>::default(),
            host_actors: <_>::default(),
            worker_actors: <_>::default(),
            worker_vm_pools: <_>::default(),
            waker: <_>::default(),
            metrics,
            workers,
            key_storage,
            scopes: scope,
            cleanup_future: None,
            root_runtime_handle: Handle::current(),
            avm_wasm_backend,
        }
    }

    /// Receives and ingests incoming particle: creates a new actor or forwards to the existing mailbox
    #[instrument(level = tracing::Level::INFO, skip_all)]
    pub fn ingest(
        &mut self,
        particle: ExtendedParticle,
        function: Option<ServiceFunction>,
        peer_scope: PeerScope,
    ) {
        let deadline = Deadline::from(particle.as_ref());
        if deadline.is_expired(now_ms()) {
            tracing::info!(target: "expired", particle_id = particle.particle.id, "Particle is expired");
            self.events
                .push_back(Err(AquamarineApiError::ParticleExpired {
                    particle_id: particle.particle.id,
                }));
            return;
        }

        if let Err(err) = particle.particle.verify() {
            tracing::warn!(target: "signature", particle_id = particle.particle.id, "Particle signature verification failed: {err:?}");
            self.events
                .push_back(Err(AquamarineApiError::SignatureVerificationFailed {
                    particle_id: particle.particle.id,
                    err,
                }));
            return;
        }

        if let PeerScope::WorkerId(worker_id) = peer_scope {
            let is_active = self.workers.is_worker_active(worker_id);
            let is_manager = self.scopes.is_management(particle.particle.init_peer_id);
            let is_host = self.scopes.is_host(particle.particle.init_peer_id);

            // Only a manager or the host itself is allowed to access deactivated workers
            if !is_active && !is_manager && !is_host {
                tracing::trace!(target: "worker_inactive", particle_id = particle.particle.id, worker_id = worker_id.to_string(), "Worker is not active");
                return;
            }
        };

        let key = ActorKey {
            signature: particle.particle.signature.clone(),
        };

        let actor = self.get_or_create_actor(peer_scope, key, &particle);

        debug_assert!(actor.is_ok(), "no such worker: {:#?}", actor.err());

        match actor {
            Ok(actor) => {
                actor.ingest(particle);
                if let Some(function) = function {
                    actor.set_function(function);
                }
            }
            Err(err) => tracing::warn!(
                "No such worker {:?}, rejected particle {particle_id}: {:?}",
                peer_scope,
                err,
                particle_id = particle.particle.id,
            ),
        }
        self.wake();
    }

    pub fn create_worker_pool(&mut self, worker_id: WorkerId, thread_count: usize) {
        let vm_pool = VmPool::new(
            thread_count,
            self.config.clone(),
            None,
            None,
            self.avm_wasm_backend.clone(),
        ); // TODO: add metrics
        self.worker_vm_pools.insert(worker_id, vm_pool);
    }

    pub fn remove_worker_pool(&mut self, worker_id: WorkerId) {
        self.worker_vm_pools.remove(&worker_id);
    }

    fn get_or_create_actor(
        &mut self,
        peer_scope: PeerScope,
        key: ActorKey,
        particle: &ExtendedParticle,
    ) -> eyre::Result<&mut Actor<RT, F>> {
        let plumber_params = PlumberParams {
            builtins: &self.builtins,
            key_storage: self.key_storage.as_ref(),
            data_store: self.data_store.clone(),
        };
        match peer_scope {
            PeerScope::Host => {
                let current_peer_id = self.scopes.get_host_peer_id();
                let spawner = Spawner::Root(RootSpawner::new(self.root_runtime_handle.clone()));
                let actor_params = ActorParams {
                    key,
                    particle,
                    peer_scope,
                    current_peer_id,
                    deal_id: None,
                    spawner,
                };
                Self::create_actor(&mut self.host_actors, plumber_params, actor_params)
            }
            PeerScope::WorkerId(worker_id) => {
                let worker_actors = match self.worker_actors.entry(worker_id) {
                    Entry::Occupied(o) => o.into_mut(),
                    Entry::Vacant(v) => v.insert(HashMap::default()),
                };
                let current_peer_id: PeerId = worker_id.into();
                let deal_id = self
                    .workers
                    .get_deal_id(worker_id)
                    .map_err(|err| eyre!("Not found deal for {:?} : {}", worker_id, err))?;
                let runtime_handle = self
                    .workers
                    .get_runtime_handle(worker_id)
                    .ok_or(eyre!("Not found runtime handle for {:?}", worker_id))?;
                let spawner = Spawner::Worker(WorkerSpawner::new(runtime_handle, worker_id));

                let actor_params = ActorParams {
                    key,
                    particle,
                    peer_scope,
                    current_peer_id,
                    deal_id: Some(deal_id),
                    spawner,
                };

                Self::create_actor(worker_actors, plumber_params, actor_params)
            }
        }
    }

    fn create_actor<'p>(
        actors: &'p mut HashMap<ActorKey, Actor<RT, F>>,
        plumber_params: PlumberParams<'p, F>,
        actor_params: ActorParams<'_>,
    ) -> eyre::Result<&'p mut Actor<RT, F>> {
        let entry = actors.entry(actor_params.key);
        let actor = match entry {
            Entry::Occupied(actor) => actor.into_mut(),
            Entry::Vacant(entry) => {
                let builtins = plumber_params.builtins;
                let key_pair = plumber_params
                    .key_storage
                    .get_keypair(actor_params.peer_scope)
                    .ok_or(eyre!(
                        "Cannot create actor, no key pair for {:?}",
                        actor_params.peer_scope
                    ))?;
                let data_store = plumber_params.data_store.clone();

                let particle_token = get_particle_token(
                    &plumber_params.key_storage.root_key_pair,
                    &actor_params.particle.particle.signature,
                )?;
                let params = ParticleParams::clone_from(
                    &actor_params.particle.particle,
                    actor_params.peer_scope,
                    particle_token.clone(),
                );
                let functions = Functions::new(params, builtins.clone());

                let actor = Actor::new(
                    &actor_params.particle.particle,
                    functions,
                    actor_params.current_peer_id,
                    particle_token,
                    key_pair,
                    data_store,
                    actor_params.deal_id,
                    actor_params.spawner,
                );
                entry.insert(actor)
            }
        };
        Ok(actor)
    }

    pub fn add_service(
        &self,
        service: String,
        functions: HashMap<String, ServiceFunction>,
        fallback: Option<ServiceFunction>,
    ) {
        let builtins = self.builtins.clone();
        let task = async move {
            builtins.extend(service, functions, fallback).await;
        };
        task::Builder::new()
            .name("Add service")
            .spawn(task)
            .expect("Could not spawn add service task");
    }

    pub fn remove_service(&self, service: String) {
        let builtins = self.builtins.clone();
        let task = async move {
            builtins.remove(&service).await;
        };
        task::Builder::new()
            .name("Remove service")
            .spawn(task)
            .expect("Could not spawn remove service task");
    }

    pub fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<RemoteRoutingEffects, AquamarineApiError>> {
        self.waker = Some(cx.waker().clone());

        self.poll_pools(cx);

        if let Some(event) = self.events.pop_front() {
            return Poll::Ready(event);
        }

        let mut remote_effects: Vec<RemoteRoutingEffects> = vec![];
        let mut local_effects: Vec<LocalRoutingEffects> = vec![];
        // Gather effects and put VMs back
        self.poll_host_actors(cx, &mut remote_effects, &mut local_effects);
        self.poll_workers_actors(cx, &mut remote_effects, &mut local_effects);

        self.cleanup(cx);

        // Execute next messages
        let host_call_stats = self.poll_next_host_messages(cx);
        let workers_call_stats = self.poll_next_worker_messages(cx);

        // TODO: separate workers and root metrics
        self.meter(|m| {
            for stat in &host_call_stats {
                m.service_call(stat.success, stat.kind, stat.call_time)
            }
            for stat in &workers_call_stats {
                m.service_call(stat.success, stat.kind, stat.call_time)
            }
        });

        for effect in local_effects {
            for local_peer in effect.next_peers {
                let span = tracing::info_span!(parent: effect.particle.span.as_ref(), "Plumber: routing effect ingest");
                let _guard = span.enter();
                self.ingest(effect.particle.clone(), None, local_peer);
            }
        }

        // Turn effects into events, and buffer them
        self.events.extend(remote_effects.into_iter().map(Ok));

        Poll::Pending
    }

    fn poll_pools(&mut self, cx: &mut Context<'_>) {
        self.host_vm_pool.poll(cx);
        for (_, vm_pool) in self.worker_vm_pools.iter_mut() {
            vm_pool.poll(cx);
        }
    }

    fn poll_host_actors(
        &mut self,
        cx: &mut Context<'_>,
        remote_effects: &mut Vec<RemoteRoutingEffects>,
        local_effects: &mut Vec<LocalRoutingEffects>,
    ) {
        let host_label =
            WorkerLabel::new(WorkerType::Host, self.scopes.get_host_peer_id().to_string());
        Self::poll_actors(
            &mut self.host_actors,
            &mut self.host_vm_pool,
            &self.scopes,
            self.metrics.as_ref(),
            cx,
            host_label,
            remote_effects,
            local_effects,
        );
    }

    fn poll_workers_actors(
        &mut self,
        cx: &mut Context<'_>,
        remote_effects: &mut Vec<RemoteRoutingEffects>,
        local_effects: &mut Vec<LocalRoutingEffects>,
    ) {
        for (worker_id, actors) in self.worker_actors.iter_mut() {
            if let Some(pool) = self.worker_vm_pools.get_mut(worker_id) {
                let peer_id: PeerId = (*worker_id).into();
                let host_label = WorkerLabel::new(WorkerType::Worker, peer_id.to_string());
                Self::poll_actors(
                    actors,
                    pool,
                    &self.scopes,
                    self.metrics.as_ref(),
                    cx,
                    host_label,
                    remote_effects,
                    local_effects,
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn poll_actors(
        actors: &mut HashMap<ActorKey, Actor<RT, F>>,
        vm_pool: &mut VmPool<RT>,
        scopes: &PeerScopes,
        metrics: Option<&ParticleExecutorMetrics>,
        cx: &mut Context<'_>,
        label: WorkerLabel,
        remote_effects: &mut Vec<RemoteRoutingEffects>,
        local_effects: &mut Vec<LocalRoutingEffects>,
    ) {
        let mut mailbox_size = 0;
        let mut interpretation_stats = vec![];

        for actor in actors.values_mut() {
            if let Poll::Ready(result) = actor.poll_completed(cx) {
                interpretation_stats.push(result.stats);

                let mut remote_peers = vec![];
                let mut local_peers = vec![];
                for next_peer in result.effects.next_peers {
                    let scope = scopes.scope(next_peer);
                    match scope {
                        Err(_) => {
                            remote_peers.push(next_peer);
                        }
                        Ok(scope) => {
                            local_peers.push(scope);
                        }
                    }
                }

                if !remote_peers.is_empty() {
                    remote_effects.push(RemoteRoutingEffects {
                        particle: result.effects.particle.clone(),
                        next_peers: remote_peers,
                    });
                }

                if !local_peers.is_empty() {
                    local_effects.push(LocalRoutingEffects {
                        particle: result.effects.particle.clone(),
                        next_peers: local_peers,
                    });
                }

                let (vm_id, vm) = result.runtime;
                if let Some(vm) = vm {
                    vm_pool.put_vm(vm_id, vm);
                } else {
                    // if `result.vm` is None, then an AVM instance was lost due to
                    // panic or cancellation, and we must ask VmPool to recreate that AVM
                    // TODO: add a Count metric to count how often we call `recreate_avm`
                    vm_pool.recreate_avm(vm_id, cx);
                }
            }
            mailbox_size += actor.mailbox_size();
        }

        if let Some(m) = metrics {
            for stat in &interpretation_stats {
                // count particle interpretations
                if stat.success {
                    m.interpretation_successes.get_or_create(&label).inc();
                } else {
                    m.interpretation_failures.get_or_create(&label).inc();
                }

                let interpretation_time = stat.interpretation_time.as_secs_f64();
                m.interpretation_time_sec
                    .get_or_create(&label)
                    .observe(interpretation_time);
            }
            m.total_actors_mailbox
                .get_or_create(&label)
                .set(mailbox_size as i64);
            m.alive_actors
                .get_or_create(&label)
                .set(actors.len() as i64);
        }
    }

    fn cleanup(&mut self, cx: &mut Context<'_>) {
        // do not schedule task if another in progress
        if let Some(Ready(())) = self.cleanup_future.as_mut().map(|f| f.poll_unpin(cx)) {
            // we remove clean up future if it is ready
            self.cleanup_future.take();
        }
        if self.cleanup_future.is_none() {
            // Remove expired actors
            let mut cleanup_keys: Vec<(String, PeerId, Vec<u8>, String)> =
                Vec::with_capacity(MAX_CLEANUP_KEYS_SIZE);
            let now = now_ms();
            self.cleanup_host_actors(&mut cleanup_keys, now);
            self.cleanup_worker_actors(&mut cleanup_keys, now);

            if !cleanup_keys.is_empty() {
                let data_store = self.data_store.clone();
                self.cleanup_future =
                    Some(async move { data_store.batch_cleanup_data(cleanup_keys).await }.boxed())
            }
        }
    }

    fn cleanup_host_actors(
        &mut self,
        cleanup_keys: &mut Vec<(String, PeerId, Vec<u8>, String)>,
        now_ms: u64,
    ) {
        Self::cleanup_actors(&mut self.host_actors, cleanup_keys, now_ms)
    }

    fn cleanup_worker_actors(
        &mut self,
        cleanup_keys: &mut Vec<(String, PeerId, Vec<u8>, String)>,
        now_ms: u64,
    ) {
        if cleanup_keys.len() >= MAX_CLEANUP_KEYS_SIZE {
            return;
        }
        self.worker_actors.retain(|worker_id, actors| {
            Self::cleanup_actors(actors, cleanup_keys, now_ms);

            !actors.is_empty() || self.worker_vm_pools.contains_key(worker_id)
        });
    }

    fn cleanup_actors(
        map: &mut HashMap<ActorKey, Actor<RT, F>>,
        cleanup_keys: &mut Vec<(String, PeerId, Vec<u8>, String)>,
        now_ms: u64,
    ) {
        map.retain(|_, actor| {
            if cleanup_keys.len() >= MAX_CLEANUP_KEYS_SIZE {
                return true;
            }
            // if actor hasn't yet expired or is still executing, keep it
            if !actor.is_expired(now_ms) || actor.is_executing() {
                return true; // keep actor
            }
            cleanup_keys.push(actor.cleanup_key());
            false // remove actor
        });
    }

    fn poll_next_host_messages(&mut self, cx: &mut Context<'_>) -> Vec<SingleCallStat> {
        let mut stats = vec![];
        for actor in self.host_actors.values_mut() {
            if let Some((vm_id, vm)) = self.host_vm_pool.get_vm() {
                match actor.poll_next(vm_id, vm, cx) {
                    ActorPoll::Vm(vm_id, vm) => self.host_vm_pool.put_vm(vm_id, vm),
                    ActorPoll::Executing(mut s) => stats.append(&mut s),
                }
            } else {
                break;
            }
        }
        stats
    }

    fn poll_next_worker_messages(&mut self, cx: &mut Context<'_>) -> Vec<SingleCallStat> {
        let mut stats = vec![];

        for (worker_id, actors) in self.worker_actors.iter_mut() {
            if let Some(pool) = self.worker_vm_pools.get_mut(worker_id) {
                for actor in actors.values_mut() {
                    if let Some((vm_id, vm)) = pool.get_vm() {
                        match actor.poll_next(vm_id, vm, cx) {
                            ActorPoll::Vm(vm_id, vm) => pool.put_vm(vm_id, vm),
                            ActorPoll::Executing(mut s) => stats.append(&mut s),
                        }
                    } else {
                        break;
                    }
                }
            }
        }
        stats
    }

    fn wake(&self) {
        if let Some(waker) = &self.waker {
            waker.wake_by_ref();
        }
    }

    fn meter<U, FF: Fn(&ParticleExecutorMetrics) -> U>(&self, f: FF) {
        self.metrics.as_ref().map(f);
    }
}

fn get_particle_token(key_pair: &KeyPair, signature: &Vec<u8>) -> eyre::Result<String> {
    let particle_token = key_pair.sign(signature.as_slice()).map_err(|err| {
        eyre!(
            "Could not produce particle token by signing the particle signature: {}",
            err
        )
    })?;
    Ok(bs58::encode(particle_token.to_vec()).into_string())
}

/// Implements `now` by taking number of non-leap seconds from `Utc::now()`
mod real_time {
    #[allow(dead_code)]
    pub fn now_ms() -> u64 {
        (chrono::Utc::now().timestamp() * 1000) as u64
    }
}

struct ActorParams<'a> {
    key: ActorKey,
    particle: &'a ExtendedParticle,
    peer_scope: PeerScope,
    current_peer_id: PeerId,
    deal_id: Option<DealId>,
    spawner: Spawner,
}

struct PlumberParams<'p, F>
where
    F: Clone,
{
    builtins: &'p F,
    key_storage: &'p KeyStorage,
    data_store: Arc<ParticleDataStore>,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::convert::Infallible;
    use std::path::PathBuf;
    use std::task::Waker;
    use std::{sync::Arc, task::Context};

    use avm_server::{AVMMemoryStats, CallResults, ParticleParameters};
    use fluence_keypair::KeyPair;
    use fluence_libp2p::RandomPeerId;
    use futures::task::noop_waker_ref;
    use workers::{DummyCoreManager, KeyStorage, PeerScopes, Workers};

    use particle_args::Args;
    use particle_execution::{FunctionOutcome, ParticleFunction, ParticleParams, ServiceFunction};
    use particle_protocol::{ExtendedParticle, Particle};

    use crate::deadline::Deadline;
    use crate::plumber::mock_time::set_mock_time;
    use crate::plumber::{now_ms, real_time};
    use crate::vm_pool::VmPool;
    use crate::AquamarineApiError::ParticleExpired;
    use crate::{AquaRuntime, ParticleDataStore, ParticleEffects, Plumber};
    use async_trait::async_trait;
    use avm_server::avm_runner::RawAVMOutcome;
    use marine_wasmtime_backend::{WasmtimeConfig, WasmtimeWasmBackend};
    use particle_services::{PeerScope, WasmBackendConfig};
    use tracing::Span;

    struct MockF;

    #[async_trait]
    impl ParticleFunction for MockF {
        async fn call(&self, _args: Args, _particle: ParticleParams) -> FunctionOutcome {
            panic!("no builtins in plumber tests!")
        }

        async fn extend(
            &self,
            _service: String,
            _functions: HashMap<String, ServiceFunction>,
            _fallback: Option<ServiceFunction>,
        ) {
            todo!()
        }

        async fn remove(&self, _service: &str) {
            todo!()
        }
    }

    struct VMMock;

    #[async_trait]
    impl AquaRuntime for VMMock {
        type Config = ();
        type Error = Infallible;

        fn create_runtime(
            _config: Self::Config,
            _backend: WasmtimeWasmBackend,
            _waker: Waker,
        ) -> Result<Self, Self::Error> {
            Ok(VMMock)
        }

        fn into_effects(
            _outcome: Result<RawAVMOutcome, Self::Error>,
            _particle_id: String,
        ) -> ParticleEffects {
            ParticleEffects {
                new_data: vec![],
                next_peers: vec![],
                call_requests: Default::default(),
            }
        }

        async fn call(
            &mut self,
            _air: impl Into<String> + Send,
            _prev_data: impl Into<Vec<u8>> + Send,
            _current_data: impl Into<Vec<u8>> + Send,
            _particle_params: ParticleParameters<'_>,
            _call_results: CallResults,
            _key_pair: &KeyPair,
        ) -> Result<RawAVMOutcome, Self::Error> {
            let soft_limits_triggering = <_>::default();
            Ok(RawAVMOutcome {
                ret_code: 0,
                error_message: "".to_string(),
                data: vec![],
                call_requests: Default::default(),
                next_peer_pks: vec![],
                soft_limits_triggering,
            })
        }

        fn memory_stats(&self) -> AVMMemoryStats {
            AVMMemoryStats {
                memory_size: 0,
                total_memory_limit: None,
                allocation_rejects: None,
            }
        }
    }

    async fn plumber() -> Plumber<VMMock, Arc<MockF>> {
        let avm_wasm_config: WasmtimeConfig = WasmBackendConfig::default().into();
        let avm_wasm_backend =
            WasmtimeWasmBackend::new(avm_wasm_config).expect("Could not create wasm backend");
        // Pool is of size 1 so it's easier to control tests
        let vm_pool = VmPool::new(1, (), None, None, avm_wasm_backend.clone());
        let builtin_mock = Arc::new(MockF);

        let root_key_pair: KeyPair = KeyPair::generate_ed25519();
        let key_pair_path: PathBuf = "keypair".into();
        let workers_path: PathBuf = "workers".into();
        let key_storage = KeyStorage::from_path(key_pair_path.clone(), root_key_pair.clone())
            .await
            .expect("Could not load key storage");

        let key_storage = Arc::new(key_storage);

        let core_manager = Arc::new(DummyCoreManager::default().into());

        let scope = PeerScopes::new(
            root_key_pair.get_peer_id(),
            RandomPeerId::random(),
            RandomPeerId::random(),
            key_storage.clone(),
        );

        let (workers, _receiver) =
            Workers::from_path(workers_path.clone(), key_storage.clone(), core_manager, 128)
                .await
                .expect("Could not load worker registry");

        let workers = Arc::new(workers);

        let tmp_dir = tempfile::tempdir().expect("Could not create temp dir");
        let tmp_path = tmp_dir.path();
        let data_store = ParticleDataStore::new(
            tmp_path.join("particles"),
            tmp_path.join("vault"),
            tmp_path.join("anomaly"),
        );
        data_store
            .initialize()
            .await
            .expect("Could not initialize datastore");
        let data_store = Arc::new(data_store);

        Plumber::new(
            (),
            vm_pool,
            data_store,
            builtin_mock,
            None,
            workers.clone(),
            key_storage.clone(),
            scope.clone(),
            avm_wasm_backend,
        )
    }

    fn particle(ts: u64, ttl: u32) -> Particle {
        let mut particle = Particle::default();
        particle.timestamp = ts;
        particle.ttl = ttl;

        particle
    }

    fn context() -> Context<'static> {
        Context::from_waker(noop_waker_ref())
    }

    /// Checks that expired actor will be removed
    #[ignore]
    #[tokio::test]
    async fn remove_expired() {
        set_mock_time(real_time::now_ms());

        let mut plumber = plumber().await;

        let particle = particle(now_ms(), 1);
        let deadline = Deadline::from(&particle);
        assert!(!deadline.is_expired(now_ms()));

        plumber.ingest(
            ExtendedParticle::new(particle, Span::none()),
            None,
            PeerScope::Host,
        );

        assert_eq!(plumber.host_actors.len(), 1);
        let mut cx = context();
        assert!(plumber.poll(&mut cx).is_pending());
        assert_eq!(plumber.host_actors.len(), 1);

        assert_eq!(plumber.host_vm_pool.free_vms(), 0);
        // pool is single VM, wait until VM is free
        loop {
            if plumber.host_vm_pool.free_vms() == 1 {
                break;
            };
            // 'is_pending' is used to suppress "must use" warning
            plumber.poll(&mut cx).is_pending();
        }

        set_mock_time(now_ms() + 2);
        assert!(plumber.poll(&mut cx).is_pending());
        assert_eq!(plumber.host_actors.len(), 0);
    }

    /// Checks that expired particle won't create an actor
    #[tokio::test]
    async fn ignore_expired() {
        set_mock_time(real_time::now_ms());
        // set_mock_time(1000);

        let mut plumber = plumber().await;
        let particle = particle(now_ms() - 100, 99);
        let deadline = Deadline::from(&particle);
        assert!(deadline.is_expired(now_ms()));

        plumber.ingest(
            ExtendedParticle::new(particle.clone(), Span::none()),
            None,
            PeerScope::Host,
        );

        assert_eq!(plumber.host_actors.len(), 0);

        // Check actor doesn't appear after poll somehow
        set_mock_time(now_ms() + 1000);
        let poll = plumber.poll(&mut context());
        assert!(poll.is_ready());
        match poll {
            std::task::Poll::Ready(Err(ParticleExpired { particle_id })) => {
                assert_eq!(particle_id, particle.id)
            }
            unexpected => panic!(
                "Expected Poll::Ready(Err(AquamarineApiError::ParticleExpired)), got {:?}",
                unexpected
            ),
        }
        assert_eq!(plumber.host_actors.len(), 0);
    }
}

/// Code taken from https://blog.iany.me/2019/03/how-to-mock-time-in-rust-tests-and-cargo-gotchas-we-met/
/// And then modified to use u64 instead of `SystemTime`
#[cfg(test)]
pub mod mock_time {
    #![allow(dead_code)]

    use std::cell::RefCell;

    thread_local! {
        static MOCK_TIME: RefCell<u64> = RefCell::new(0);
    }

    pub fn now_ms() -> u64 {
        MOCK_TIME.with(|cell| *cell.borrow())
    }

    pub fn set_mock_time(time: u64) {
        MOCK_TIME.with(|cell| *cell.borrow_mut() = time);
    }
}
