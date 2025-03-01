/*
 * Copyright 2022 Fluence Labs Limited
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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::{FutureExt, StreamExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::spell_builtins::{
    get_spell_arg, get_spell_id, spell_install, spell_list, spell_remove, spell_update_config,
    store_error, store_response,
};
use crate::worker_builins::{
    activate_deal, create_worker, deactivate_deal, get_worker_peer_id, is_deal_active,
    remove_worker, worker_list,
};
use aquamarine::AquamarineApi;
use particle_args::JError;
use particle_builtins::{wrap, wrap_unit, CustomService};
use particle_execution::ServiceFunction;
use particle_modules::ModuleRepository;
use particle_services::{ParticleAppServices, PeerScope};
use peer_metrics::SpellMetrics;
use serde_json::Value;
use server_config::ResolvedConfig;
use spell_event_bus::api::{from_user_config, SpellEventBusApi, TriggerEvent};
use spell_service_api::{CallParams, SpellServiceApi};
use spell_storage::SpellStorage;
use tracing::Instrument;
use workers::{KeyStorage, PeerScopes, Workers};

#[derive(Clone)]
pub struct Sorcerer {
    pub aquamarine: AquamarineApi,
    pub services: ParticleAppServices,
    pub spell_storage: SpellStorage,
    pub spell_event_bus_api: SpellEventBusApi,
    pub spell_script_particle_ttl: Duration,
    pub workers: Arc<Workers>,
    pub key_storage: Arc<KeyStorage>,
    pub scopes: PeerScopes,
    pub spell_service_api: SpellServiceApi,
    pub spell_metrics: Option<SpellMetrics>,
    pub worker_period_sec: u32,
}

impl Sorcerer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        services: ParticleAppServices,
        modules: ModuleRepository,
        aquamarine: AquamarineApi,
        config: ResolvedConfig,
        spell_event_bus_api: SpellEventBusApi,
        workers: Arc<Workers>,
        key_storage: Arc<KeyStorage>,
        scope: PeerScopes,
        spell_service_api: SpellServiceApi,
        spell_metrics: Option<SpellMetrics>,
    ) -> (Self, HashMap<String, CustomService>, String) {
        let (spell_storage, spell_version) =
            SpellStorage::create(&config.dir_config.spell_base_dir, &services, &modules)
                .expect("Spell storage creation");

        let sorcerer = Self {
            aquamarine,
            services,
            spell_storage,
            spell_event_bus_api,
            spell_script_particle_ttl: config.max_spell_particle_ttl,
            workers,
            key_storage,
            scopes: scope,
            spell_service_api,
            spell_metrics,
            worker_period_sec: config.system_services.decider.worker_period_sec,
        };

        let mut builtin_functions = sorcerer.make_spell_builtins();
        builtin_functions.extend_one(sorcerer.make_worker_builtin());

        (sorcerer, builtin_functions, spell_version)
    }

    async fn resubscribe_spells(&self) {
        for spell_id in self
            .spell_storage
            .get_registered_spells()
            .values()
            .flatten()
        {
            log::info!("Rescheduling spell {}", spell_id);
            let result: Result<(), JError> = try {
                let spell_owner =
                    self.services
                        .get_service_owner(PeerScope::Host, spell_id.clone(), "")?;
                let peer_scope = self
                    .scopes
                    .scope(spell_owner)
                    .expect("Should be local peer_id");
                let params = CallParams::local(
                    peer_scope,
                    spell_id.clone(),
                    spell_owner,
                    self.spell_script_particle_ttl,
                );
                let config = self.spell_service_api.get_trigger_config(params)?;
                let period = config.clock.period_sec;
                let config = from_user_config(&config)?;
                if let Some(config) = config.and_then(|c| c.into_rescheduled()) {
                    self.spell_event_bus_api
                        .subscribe(spell_id.clone(), config)
                        .await?;
                    if let Some(m) = &self.spell_metrics {
                        m.observe_started_spell(period);
                    }
                } else {
                    log::warn!("Spell {spell_id} is not rescheduled since its config is either not found or not reschedulable");
                }
            };
            if let Err(e) = result {
                // 1. We do not remove the spell we aren't able to reschedule. Users should be able to rerun it manually when updating trigger config.
                // 2. Maybe we should somehow register which spell are running and which are not and notify user about it.
                log::warn!("Failed to reschedule spell {}: {}.", spell_id, e);
            }
        }
    }

    pub fn start(
        self,
        spell_events_receiver: mpsc::UnboundedReceiver<TriggerEvent>,
    ) -> JoinHandle<()> {
        tokio::task::Builder::new()
            .name("sorcerer")
            .spawn(async {
                self.resubscribe_spells().await;
                let spell_events_stream = UnboundedReceiverStream::new(spell_events_receiver);
                spell_events_stream
                    .for_each_concurrent(None, move |spell_event| {
                        let root_span = tracing::info_span!(
                            "Sorcerer::task::for_each",
                            spell_id = spell_event.spell_id.to_string()
                        );
                        let root_span = Arc::new(root_span);
                        let async_span = tracing::info_span!(parent: root_span.as_ref(),
                            "Sorcerer::task::execute_script",
                            spell_id = spell_event.spell_id.to_string());

                        let sorcerer = self.clone();
                        // Note that the event that triggered the spell is in `spell_event.event`
                        async move {
                            sorcerer
                                .execute_script(spell_event, root_span)
                                .in_current_span()
                                .await;
                        }
                        .instrument(async_span)
                    })
                    .await;
            })
            .expect("Could not spawn task")
    }

    fn make_spell_builtins(&self) -> HashMap<String, CustomService> {
        let mut spell_builtins: HashMap<String, CustomService> = HashMap::new();

        spell_builtins.insert(
            "spell".to_string(),
            CustomService::new(
                vec![
                    ("install", self.make_spell_install_closure()),
                    ("remove", self.make_spell_remove_closure()),
                    ("list", self.make_spell_list_closure()),
                    (
                        "update_trigger_config",
                        self.make_spell_update_config_closure(),
                    ),
                ],
                None,
            ),
        );

        spell_builtins.insert(
            "getDataSrv".to_string(),
            CustomService::new(
                vec![
                    ("spell_id", self.make_get_spell_id_closure()),
                    ("-relay-", self.make_get_relay_closure()),
                ],
                Some(self.make_get_spell_arg_closure()),
            ),
        );

        spell_builtins.insert(
            "errorHandlingSrv".to_string(),
            CustomService::new(vec![("error", self.make_error_handler_closure())], None),
        );

        spell_builtins.insert(
            "callbackSrv".to_string(),
            CustomService::new(
                vec![("response", self.make_response_handler_closure())],
                None,
            ),
        );

        spell_builtins
    }

    fn make_worker_builtin(&self) -> (String, CustomService) {
        (
            "worker".to_string(),
            CustomService::new(
                vec![
                    ("create", self.make_worker_create_closure()),
                    ("get_worker_id", self.make_worker_get_worker_id_closure()),
                    ("remove", self.make_worker_remove_closure()),
                    ("list", self.make_worker_list_closure()),
                    ("activate", self.make_activate_deal_closure()),
                    ("deactivate", self.make_deactivate_deal_closure()),
                    ("is_active", self.make_is_deal_active_closure()),
                ],
                None,
            ),
        )
    }

    fn make_spell_install_closure(&self) -> ServiceFunction {
        let services = self.services.clone();
        let storage = self.spell_storage.clone();
        let spell_event_bus = self.spell_event_bus_api.clone();
        let workers = self.workers.clone();
        let spell_service_api = self.spell_service_api.clone();
        let scope = self.scopes.clone();
        ServiceFunction::Immut(Box::new(move |args, params| {
            let storage = storage.clone();
            let services = services.clone();
            let spell_event_bus_api = spell_event_bus.clone();
            let spell_service_api = spell_service_api.clone();
            let workers = workers.clone();
            let scope = scope.clone();
            async move {
                wrap(
                    spell_install(
                        args,
                        params,
                        storage,
                        services,
                        spell_event_bus_api,
                        spell_service_api,
                        workers,
                        scope,
                    )
                    .await,
                )
            }
            .boxed()
        }))
    }

    fn make_spell_remove_closure(&self) -> ServiceFunction {
        let services = self.services.clone();
        let storage = self.spell_storage.clone();
        let spell_event_bus_api = self.spell_event_bus_api.clone();
        let workers = self.workers.clone();
        let scopes = self.scopes.clone();

        ServiceFunction::Immut(Box::new(move |args, params| {
            let storage = storage.clone();
            let services = services.clone();
            let api = spell_event_bus_api.clone();
            let workers = workers.clone();
            let scopes = scopes.clone();
            async move {
                let result =
                    spell_remove(args, params, storage, services, api, workers, scopes).await;
                wrap_unit(result)
            }
            .boxed()
        }))
    }

    fn make_spell_list_closure(&self) -> ServiceFunction {
        let storage = self.spell_storage.clone();
        ServiceFunction::Immut(Box::new(move |_, params| {
            let storage = storage.clone();
            async move { wrap(spell_list(params, storage)) }.boxed()
        }))
    }

    fn make_spell_update_config_closure(&self) -> ServiceFunction {
        let spell_event_bus_api = self.spell_event_bus_api.clone();
        let services = self.services.clone();
        let workers = self.workers.clone();
        let scope = self.scopes.clone();
        let spell_service_api = self.spell_service_api.clone();
        ServiceFunction::Immut(Box::new(move |args, params| {
            let spell_event_bus_api = spell_event_bus_api.clone();
            let services = services.clone();
            let spell_service_api = spell_service_api.clone();
            let workers = workers.clone();
            let scopes = scope.clone();
            async move {
                wrap_unit(
                    spell_update_config(
                        args,
                        params,
                        services,
                        spell_event_bus_api,
                        spell_service_api,
                        workers,
                        scopes,
                    )
                    .await,
                )
            }
            .boxed()
        }))
    }

    fn make_get_spell_id_closure(&self) -> ServiceFunction {
        ServiceFunction::Immut(Box::new(move |_, params| {
            async move { wrap(get_spell_id(params)) }.boxed()
        }))
    }

    fn make_get_relay_closure(&self) -> ServiceFunction {
        let relay_peer_id = self.scopes.get_host_peer_id().to_base58();
        ServiceFunction::Immut(Box::new(move |_, _| {
            let relay = relay_peer_id.clone();
            async move { wrap(Ok(Value::String(relay))) }.boxed()
        }))
    }

    fn make_get_spell_arg_closure(&self) -> ServiceFunction {
        let spell_service_api = self.spell_service_api.clone();
        ServiceFunction::Immut(Box::new(move |args, params| {
            let spell_service_api = spell_service_api.clone();
            async move { wrap(get_spell_arg(args, params, spell_service_api)) }.boxed()
        }))
    }

    fn make_error_handler_closure(&self) -> ServiceFunction {
        let spell_service_api = self.spell_service_api.clone();
        ServiceFunction::Immut(Box::new(move |args, params| {
            let spell_service_api = spell_service_api.clone();
            async move { wrap_unit(store_error(args, params, spell_service_api)) }.boxed()
        }))
    }

    fn make_response_handler_closure(&self) -> ServiceFunction {
        let spell_service_api = self.spell_service_api.clone();
        ServiceFunction::Immut(Box::new(move |args, params| {
            let spell_service_api = spell_service_api.clone();
            async move { wrap_unit(store_response(args, params, spell_service_api)) }.boxed()
        }))
    }

    fn make_worker_create_closure(&self) -> ServiceFunction {
        let workers = self.workers.clone();
        ServiceFunction::Immut(Box::new(move |args, params| {
            let workers = workers.clone();
            async move {
                let res: Result<Value, JError> = create_worker(args, params, workers).await;
                wrap(res)
            }
            .boxed()
        }))
    }

    fn make_worker_get_worker_id_closure(&self) -> ServiceFunction {
        let workers = self.workers.clone();
        ServiceFunction::Immut(Box::new(move |args, _| {
            let workers = workers.clone();
            async move { wrap(get_worker_peer_id(args, workers)) }.boxed()
        }))
    }

    fn make_worker_list_closure(&self) -> ServiceFunction {
        let workers = self.workers.clone();
        ServiceFunction::Immut(Box::new(move |_, _| {
            let workers = workers.clone();
            async move { wrap(worker_list(workers)) }.boxed()
        }))
    }

    fn make_worker_remove_closure(&self) -> ServiceFunction {
        let services = self.services.clone();
        let storage = self.spell_storage.clone();
        let spell_event_bus_api = self.spell_event_bus_api.clone();
        let workers = self.workers.clone();
        let scopes = self.scopes.clone();

        ServiceFunction::Immut(Box::new(move |args, params| {
            let storage = storage.clone();
            let services = services.clone();
            let api = spell_event_bus_api.clone();
            let workers = workers.clone();
            let scopes = scopes.clone();
            async move {
                let res =
                    remove_worker(args, params, workers, services, storage, api, scopes).await;
                wrap_unit(res)
            }
            .boxed()
        }))
    }

    fn make_activate_deal_closure(&self) -> ServiceFunction {
        let workers = self.workers.clone();
        let scope = self.scopes.clone();
        let services = self.services.clone();
        let spell_event_bus_api = self.spell_event_bus_api.clone();
        let spells_api = self.spell_service_api.clone();
        let worker_period_sec = self.worker_period_sec;
        ServiceFunction::Immut(Box::new(move |args, params| {
            let services = services.clone();
            let spell_event_bus_api = spell_event_bus_api.clone();
            let spells_api = spells_api.clone();
            let workers = workers.clone();
            let scope = scope.clone();

            async move {
                let res = activate_deal(
                    args,
                    params,
                    workers,
                    scope,
                    services,
                    spell_event_bus_api,
                    spells_api,
                    worker_period_sec,
                )
                .await;
                wrap_unit(res)
            }
            .boxed()
        }))
    }

    fn make_deactivate_deal_closure(&self) -> ServiceFunction {
        let workers = self.workers.clone();
        let scope = self.scopes.clone();
        let spell_storage = self.spell_storage.clone();
        let spell_event_bus_api = self.spell_event_bus_api.clone();
        let spells_api = self.spell_service_api.clone();

        ServiceFunction::Immut(Box::new(move |args, params| {
            let spells_api = spells_api.clone();
            let spell_storage = spell_storage.clone();
            let spell_event_bus_api = spell_event_bus_api.clone();
            let workers = workers.clone();
            let scope = scope.clone();

            async move {
                let res = deactivate_deal(
                    args,
                    params,
                    workers,
                    scope,
                    spell_storage,
                    spell_event_bus_api,
                    spells_api,
                )
                .await;
                wrap_unit(res)
            }
            .boxed()
        }))
    }

    fn make_is_deal_active_closure(&self) -> ServiceFunction {
        let workers = self.workers.clone();
        ServiceFunction::Immut(Box::new(move |args, _| {
            let workers = workers.clone();
            async move {
                tokio::task::spawn_blocking(move || wrap(is_deal_active(args, workers))).await?
            }
            .boxed()
        }))
    }
}
