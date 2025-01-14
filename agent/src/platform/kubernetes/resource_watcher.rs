/*
 * Copyright (c) 2022 Yunshan Networks
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

use std::{
    collections::HashMap,
    fmt::Debug,
    io::{self, Write},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Weak,
    },
    time::{Duration, Instant, SystemTime},
};

use enum_dispatch::enum_dispatch;
use flate2::{write::ZlibEncoder, Compression};
use futures::{StreamExt, TryStreamExt};
use k8s_openapi::{
    api::{
        apps::v1::{
            DaemonSet, DaemonSetSpec, Deployment, DeploymentSpec, ReplicaSet, ReplicaSetSpec,
            StatefulSet, StatefulSetSpec,
        },
        core::v1::{
            Namespace, Node, NodeSpec, NodeStatus, Pod, PodStatus, ReplicationController,
            ReplicationControllerSpec, Service, ServiceSpec,
        },
        extensions, networking,
    },
    apimachinery::pkg::apis::meta::v1::ObjectMeta,
    Metadata,
};
use kube::{
    api::ListParams,
    runtime::{self, watcher::Event},
    Api, Client, Resource,
};
use log::{debug, info, warn};
use openshift_openapi::api::route::v1::Route;
use serde::de::DeserializeOwned;
use serde::ser::Serialize;
use tokio::{runtime::Handle, sync::Mutex, task::JoinHandle, time};

use crate::utils::stats::{
    self, Countable, Counter, CounterType, CounterValue, RefCountable, StatsOption,
};

const LIST_INTERVAL: Duration = Duration::from_secs(600);
const REFRESH_INTERVAL: Duration = Duration::from_secs(3600);
const SLEEP_INTERVAL: Duration = Duration::from_secs(5);

#[enum_dispatch]
pub trait Watcher {
    fn start(&self) -> Option<JoinHandle<()>>;
    fn error(&self) -> Option<String>;
    fn entries(&self) -> Vec<Vec<u8>>;
    fn kind(&self) -> String;
    fn version(&self) -> u64;
    fn ready(&self) -> bool;
}

#[enum_dispatch(Watcher)]
#[derive(Clone)]
pub enum GenericResourceWatcher {
    Node(ResourceWatcher<Node>),
    Namespace(ResourceWatcher<Namespace>),
    Service(ResourceWatcher<Service>),
    Deployment(ResourceWatcher<Deployment>),
    Pod(ResourceWatcher<Pod>),
    StatefulSet(ResourceWatcher<StatefulSet>),
    DaemonSet(ResourceWatcher<DaemonSet>),
    ReplicationController(ResourceWatcher<ReplicationController>),
    ReplicaSet(ResourceWatcher<ReplicaSet>),
    V1Ingress(ResourceWatcher<networking::v1::Ingress>),
    V1beta1Ingress(ResourceWatcher<networking::v1beta1::Ingress>),
    ExtV1beta1Ingress(ResourceWatcher<extensions::v1beta1::Ingress>),
    Route(ResourceWatcher<Route>),
}

#[derive(Default)]
pub struct WatcherCounter {
    list_count: AtomicU32,
    list_length: AtomicU32,
    list_cost_time_sum: AtomicU64, // ns
    list_error: AtomicU32,
    watch_applied: AtomicU32,
    watch_deleted: AtomicU32,
    watch_restarted: AtomicU32,
}

impl RefCountable for WatcherCounter {
    fn get_counters(&self) -> Vec<Counter> {
        let list_count = self.list_count.swap(0, Ordering::Relaxed);
        let list_avg_cost_time = self
            .list_cost_time_sum
            .swap(0, Ordering::Relaxed)
            .checked_div(list_count as u64)
            .unwrap_or_default();
        let list_avg_length = self
            .list_length
            .swap(0, Ordering::Relaxed)
            .checked_div(list_count)
            .unwrap_or_default();
        vec![
            (
                "list_avg_length",
                CounterType::Gauged,
                CounterValue::Unsigned(list_avg_length as u64),
            ),
            (
                "list_avg_cost_time",
                CounterType::Counted,
                CounterValue::Unsigned(list_avg_cost_time),
            ),
            (
                "list_error",
                CounterType::Gauged,
                CounterValue::Unsigned(self.list_error.swap(0, Ordering::Relaxed) as u64),
            ),
            (
                "watch_applied",
                CounterType::Gauged,
                CounterValue::Unsigned(self.watch_applied.swap(0, Ordering::Relaxed) as u64),
            ),
            (
                "watch_deleted",
                CounterType::Gauged,
                CounterValue::Unsigned(self.watch_deleted.swap(0, Ordering::Relaxed) as u64),
            ),
            (
                "watch_restarted",
                CounterType::Gauged,
                CounterValue::Unsigned(self.watch_restarted.swap(0, Ordering::Relaxed) as u64),
            ),
        ]
    }
}

// 发生错误，需要重新构造实例
#[derive(Clone)]
pub struct ResourceWatcher<K> {
    api: Api<K>,
    entries: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    err_msg: Arc<Mutex<Option<String>>>,
    kind: &'static str,
    version: Arc<AtomicU64>,
    runtime: Handle,
    ready: Arc<AtomicBool>,
    stats_counter: Arc<WatcherCounter>,
}

struct ProcessArg<K> {
    entries: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    version: Arc<AtomicU64>,
    api: Api<K>,
    kind: &'static str,
    err_msg: Arc<Mutex<Option<String>>>,
    ready: Arc<AtomicBool>,
    stats_counter: Arc<WatcherCounter>,
}

impl<K> Watcher for ResourceWatcher<K>
where
    K: Clone + Debug + DeserializeOwned + Resource + Serialize + Trimmable,
    K: Metadata<Ty = ObjectMeta>,
{
    fn start(&self) -> Option<JoinHandle<()>> {
        let args = ProcessArg {
            entries: self.entries.clone(),
            version: self.version.clone(),
            kind: self.kind,
            err_msg: self.err_msg.clone(),
            ready: self.ready.clone(),
            api: self.api.clone(),
            stats_counter: self.stats_counter.clone(),
        };

        let handle = self.runtime.spawn(Self::process(args));
        info!("{} watcher started", self.kind);
        Some(handle)
    }

    fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    fn error(&self) -> Option<String> {
        self.err_msg.blocking_lock().take()
    }

    fn kind(&self) -> String {
        self.kind.to_string()
    }

    fn entries(&self) -> Vec<Vec<u8>> {
        self.entries
            .blocking_lock()
            .values()
            .map(Clone::clone)
            .collect::<Vec<_>>()
    }

    fn ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }
}

impl<K> ResourceWatcher<K>
where
    K: Clone + Debug + DeserializeOwned + Resource + Serialize + Trimmable,
    K: Metadata<Ty = ObjectMeta>,
{
    pub fn new(api: Api<K>, kind: &'static str, runtime: Handle) -> Self {
        Self {
            api,
            entries: Arc::new(Mutex::new(HashMap::new())),
            version: Arc::new(AtomicU64::new(0)),
            kind,
            err_msg: Arc::new(Mutex::new(None)),
            runtime,
            ready: Default::default(),
            stats_counter: Default::default(),
        }
    }

    async fn process(args: ProcessArg<K>) {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        Self::get_list_entry(&args, &mut encoder).await;
        args.ready.store(true, Ordering::Relaxed);
        info!("{} watcher ready", args.kind);

        let mut last_update = SystemTime::now();
        let mut stream = runtime::watcher(args.api.clone(), ListParams::default()).boxed();

        // If the watch is successful, keep updating the entry with the watch. If the watch is not successful,
        // update the entry with the full amount every 10 minutes.
        loop {
            while let Ok(Some(event)) = stream.try_next().await {
                Self::resolve_event(&args, &mut encoder, event).await;
            }
            if last_update.elapsed().unwrap() >= LIST_INTERVAL {
                Self::full_sync(&args, &mut encoder).await;
                last_update = SystemTime::now();
            }
            time::sleep(SLEEP_INTERVAL).await;
        }
    }

    async fn full_sync(args: &ProcessArg<K>, encoder: &mut ZlibEncoder<Vec<u8>>) {
        let now = Instant::now();
        Self::get_list_entry(args, encoder).await;
        args.stats_counter
            .list_cost_time_sum
            .fetch_add(now.elapsed().as_nanos() as u64, Ordering::Relaxed);
        args.stats_counter
            .list_count
            .fetch_add(1, Ordering::Relaxed);
    }

    async fn get_list_entry(args: &ProcessArg<K>, encoder: &mut ZlibEncoder<Vec<u8>>) {
        match args.api.list(&ListParams::default()).await {
            Ok(object_list) => {
                args.stats_counter
                    .list_length
                    .fetch_add(object_list.items.len() as u32, Ordering::Relaxed);
                info!(
                    "k8s {} watcher list entry.len={}",
                    args.kind,
                    object_list.items.len()
                );
                // 检查内存和List API查询结果是否一致
                {
                    let entries_lock = args.entries.lock().await;
                    if object_list.items.len() == entries_lock.len() {
                        let mut identical = true;
                        for object in object_list.items.iter() {
                            match object.meta().uid.as_ref() {
                                Some(uid) if entries_lock.contains_key(uid) => (),
                                _ => {
                                    identical = false;
                                    break;
                                }
                            }
                        }
                        if identical {
                            return;
                        }
                    }
                }

                debug!("reload {} data", args.kind);

                let mut new_entries = HashMap::new();

                for object in object_list {
                    if object.meta().uid.as_ref().is_none() {
                        continue;
                    }
                    let mut trim_object = object.trim();
                    match serde_json::to_vec(&trim_object) {
                        Ok(serialized_object) => {
                            let compressed_object =
                                match Self::compress_entry(encoder, serialized_object.as_slice()) {
                                    Ok(c) => c,
                                    Err(e) => {
                                        warn!(
                                        "failed to compress {} resource with UID({}) error: {} ",
                                        args.kind,
                                        trim_object.meta().uid.as_ref().unwrap(),
                                        e
                                    );
                                        continue;
                                    }
                                };
                            new_entries.insert(
                                trim_object.meta_mut().uid.take().unwrap(),
                                compressed_object,
                            );
                        }
                        Err(e) => warn!(
                            "failed serialized resource {} UID({}) to json Err: {}",
                            args.kind,
                            trim_object.meta().uid.as_ref().unwrap(),
                            e
                        ),
                    }
                }

                if !new_entries.is_empty() {
                    *args.entries.lock().await = new_entries;
                    args.version.fetch_add(1, Ordering::SeqCst);
                }
            }
            Err(err) => {
                args.stats_counter
                    .list_error
                    .fetch_add(1, Ordering::Relaxed);
                let msg = format!("{} watcher list failed: {}", args.kind, err);
                warn!("{}", msg);
                args.err_msg.lock().await.replace(msg);
            }
        }
    }

    async fn resolve_event(
        args: &ProcessArg<K>,
        encoder: &mut ZlibEncoder<Vec<u8>>,
        event: Event<K>,
    ) {
        match event {
            Event::Applied(object) => {
                Self::insert_object(encoder, object, &args.entries, &args.version, &args.kind)
                    .await;
                args.stats_counter
                    .watch_applied
                    .fetch_add(1, Ordering::Relaxed);
            }
            Event::Deleted(mut object) => {
                if let Some(uid) = object.meta_mut().uid.take() {
                    // 只有删除时检查是否需要更新版本号，其余消息直接更新map内容
                    if args.entries.lock().await.remove(&uid).is_some() {
                        args.version.fetch_add(1, Ordering::SeqCst);
                    }
                    args.stats_counter
                        .watch_deleted
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            Event::Restarted(mut objects) => {
                // 按照语义重启后应该拿改key对应最新的state，所以只取restart的最后一个
                // restarted 存储的是某个key对应的object在重启过程中不同状态
                if let Some(object) = objects.pop() {
                    Self::insert_object(encoder, object, &args.entries, &args.version, &args.kind)
                        .await;
                    args.stats_counter
                        .watch_restarted
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    async fn insert_object(
        encoder: &mut ZlibEncoder<Vec<u8>>,
        object: K,
        entries: &Arc<Mutex<HashMap<String, Vec<u8>>>>,
        version: &Arc<AtomicU64>,
        kind: &str,
    ) {
        let uid = object.meta().uid.clone();
        if let Some(uid) = uid {
            let trim_object = object.trim();
            let serialized_object = serde_json::to_vec(&trim_object);
            match serialized_object {
                Ok(serobj) => {
                    let compressed_object = match Self::compress_entry(encoder, serobj.as_slice()) {
                        Ok(c) => c,
                        Err(e) => {
                            warn!(
                                "failed to compress {} resource with UID({}) error: {} ",
                                kind,
                                trim_object.meta().uid.as_ref().unwrap(),
                                e
                            );
                            return;
                        }
                    };
                    entries.lock().await.insert(uid, compressed_object);
                    version.fetch_add(1, Ordering::SeqCst);
                }
                Err(e) => debug!(
                    "failed serialized resource {} UID({}) to json Err: {}",
                    kind, uid, e
                ),
            }
        }
    }

    fn compress_entry(encoder: &mut ZlibEncoder<Vec<u8>>, entry: &[u8]) -> io::Result<Vec<u8>> {
        encoder.write_all(entry)?;
        encoder.reset(vec![])
    }
}

pub trait Trimmable: 'static + Send {
    fn trim(self) -> Self;
}

impl Trimmable for Pod {
    fn trim(mut self) -> Self {
        let mut trim_pod = Pod::default();
        trim_pod.metadata = ObjectMeta {
            uid: self.metadata.uid.take(),
            name: self.metadata.name.take(),
            namespace: self.metadata.namespace.take(),
            owner_references: self.metadata.owner_references.take(),
            creation_timestamp: self.metadata.creation_timestamp.take(),
            labels: self.metadata.labels.take(),
            ..Default::default()
        };
        if let Some(pod_status) = self.status.take() {
            trim_pod.status = Some(PodStatus {
                host_ip: pod_status.host_ip,
                conditions: pod_status.conditions,
                pod_ip: pod_status.pod_ip,
                ..Default::default()
            });
        }
        trim_pod
    }
}

impl Trimmable for Node {
    fn trim(mut self) -> Self {
        let mut trim_node = Node::default();
        trim_node.metadata = ObjectMeta {
            uid: self.metadata.uid.take(),
            name: self.metadata.name.take(),
            labels: self.metadata.labels.take(),
            ..Default::default()
        };

        if let Some(node_status) = self.status.take() {
            trim_node.status = Some(NodeStatus {
                addresses: node_status.addresses,
                conditions: node_status.conditions,
                capacity: node_status.capacity,
                ..Default::default()
            });
        }
        if let Some(node_spec) = self.spec.take() {
            trim_node.spec = Some(NodeSpec {
                pod_cidr: node_spec.pod_cidr,
                ..Default::default()
            });
        }
        trim_node
    }
}

impl Trimmable for ReplicaSet {
    fn trim(mut self) -> Self {
        let mut trim_rs = ReplicaSet::default();
        trim_rs.metadata = ObjectMeta {
            uid: self.metadata.uid.take(),
            name: self.metadata.name.take(),
            namespace: self.metadata.namespace.take(),
            owner_references: self.metadata.owner_references.take(),
            labels: self.metadata.labels.take(),
            ..Default::default()
        };

        if let Some(rs_spec) = self.spec.take() {
            trim_rs.spec = Some(ReplicaSetSpec {
                replicas: rs_spec.replicas,
                selector: rs_spec.selector,
                ..Default::default()
            });
        }

        trim_rs
    }
}

impl Trimmable for ReplicationController {
    fn trim(mut self) -> Self {
        let mut trim_rc = ReplicationController::default();
        trim_rc.metadata = ObjectMeta {
            uid: self.metadata.uid.take(),
            name: self.metadata.name.take(),
            namespace: self.metadata.namespace.take(),
            ..Default::default()
        };

        if let Some(rc_spec) = self.spec.take() {
            trim_rc.spec = Some(ReplicationControllerSpec {
                replicas: rc_spec.replicas,
                selector: rc_spec.selector,
                template: rc_spec.template,
                ..Default::default()
            });
        }

        trim_rc
    }
}

impl Trimmable for networking::v1::Ingress {
    fn trim(mut self) -> Self {
        self.metadata = ObjectMeta {
            uid: self.metadata.uid.take(),
            name: self.metadata.name.take(),
            namespace: self.metadata.namespace.take(),
            ..Default::default()
        };
        self.status = None;
        self
    }
}

impl Trimmable for networking::v1beta1::Ingress {
    fn trim(mut self) -> Self {
        self.metadata = ObjectMeta {
            uid: self.metadata.uid.take(),
            name: self.metadata.name.take(),
            namespace: self.metadata.namespace.take(),
            ..Default::default()
        };
        self.status = None;
        self
    }
}

impl Trimmable for extensions::v1beta1::Ingress {
    fn trim(mut self) -> Self {
        self.metadata = ObjectMeta {
            uid: self.metadata.uid.take(),
            name: self.metadata.name.take(),
            namespace: self.metadata.namespace.take(),
            ..Default::default()
        };
        self.status = None;
        self
    }
}

impl Trimmable for Route {
    fn trim(mut self) -> Self {
        self.metadata = ObjectMeta {
            uid: self.metadata.uid.take(),
            name: self.metadata.name.take(),
            namespace: self.metadata.namespace.take(),
            ..Default::default()
        };
        self.status = Default::default();
        self
    }
}

impl Trimmable for DaemonSet {
    fn trim(mut self) -> Self {
        let mut trim_ds = DaemonSet::default();
        trim_ds.metadata = ObjectMeta {
            uid: self.metadata.uid.take(),
            name: self.metadata.name.take(),
            namespace: self.metadata.namespace.take(),
            labels: self.metadata.labels.take(),
            ..Default::default()
        };
        if let Some(ds_spec) = self.spec.take() {
            trim_ds.spec = Some(DaemonSetSpec {
                selector: ds_spec.selector,
                template: ds_spec.template,
                ..Default::default()
            })
        }

        trim_ds
    }
}

impl Trimmable for StatefulSet {
    fn trim(mut self) -> Self {
        let mut trim_st = StatefulSet::default();
        trim_st.metadata = ObjectMeta {
            uid: self.metadata.uid.take(),
            name: self.metadata.name.take(),
            namespace: self.metadata.namespace.take(),
            labels: self.metadata.labels.take(),
            ..Default::default()
        };

        if let Some(st_spec) = self.spec.take() {
            trim_st.spec = Some(StatefulSetSpec {
                replicas: st_spec.replicas,
                selector: st_spec.selector,
                template: st_spec.template,
                ..Default::default()
            })
        }
        trim_st
    }
}

impl Trimmable for Deployment {
    fn trim(mut self) -> Self {
        let mut trim_de = Deployment::default();
        trim_de.metadata = ObjectMeta {
            uid: self.metadata.uid.take(),
            name: self.metadata.name.take(),
            namespace: self.metadata.namespace.take(),
            labels: self.metadata.labels.take(),
            ..Default::default()
        };

        if let Some(de_spec) = self.spec.take() {
            trim_de.spec = Some(DeploymentSpec {
                replicas: de_spec.replicas,
                selector: de_spec.selector,
                template: de_spec.template,
                ..Default::default()
            });
        }

        trim_de
    }
}

impl Trimmable for Service {
    fn trim(mut self) -> Self {
        let mut trim_svc = Service::default();
        trim_svc.metadata = ObjectMeta {
            uid: self.metadata.uid.take(),
            name: self.metadata.name.take(),
            namespace: self.metadata.namespace.take(),
            annotations: self.metadata.annotations.take(),
            ..Default::default()
        };

        if let Some(svc_spec) = self.spec.take() {
            trim_svc.spec = Some(ServiceSpec {
                selector: svc_spec.selector,
                type_: svc_spec.type_,
                cluster_ip: svc_spec.cluster_ip,
                ports: svc_spec.ports,
                ..Default::default()
            });
        }
        trim_svc
    }
}

impl Trimmable for Namespace {
    fn trim(mut self) -> Self {
        let mut trim_ns = Namespace::default();
        trim_ns.metadata = ObjectMeta {
            uid: self.metadata.uid.take(),
            name: self.metadata.name.take(),
            ..Default::default()
        };
        trim_ns
    }
}

pub struct ResourceWatcherFactory {
    client: Client,
    runtime: Handle,
}

impl ResourceWatcherFactory {
    pub fn new(client: Client, runtime: Handle) -> Self {
        Self { client, runtime }
    }

    fn new_watcher_inner<K>(
        &self,
        kind: &'static str,
        stats_collector: &stats::Collector,
        namespace: Option<&str>,
    ) -> ResourceWatcher<K>
    where
        K: Clone + Debug + DeserializeOwned + Resource + Serialize + Trimmable,
        K: Metadata<Ty = ObjectMeta>,
        <K as Resource>::DynamicType: Default,
    {
        let watcher = ResourceWatcher::new(
            match namespace {
                Some(namespace) => Api::namespaced(self.client.clone(), namespace),
                None => Api::all(self.client.clone()),
            },
            kind,
            self.runtime.clone(),
        );
        stats_collector.register_countable(
            "resource_watcher",
            Countable::Ref(Arc::downgrade(&watcher.stats_counter) as Weak<dyn RefCountable>),
            vec![StatsOption::Tag("kind", watcher.kind.to_string())],
        );
        watcher
    }

    pub fn new_watcher(
        &self,
        resource: &'static str,
        kind: &'static str,
        namespace: Option<&str>,
        stats_collector: &stats::Collector,
    ) -> Option<GenericResourceWatcher> {
        let watcher = match resource {
            // 特定namespace不支持Node/Namespace资源
            "nodes" => {
                GenericResourceWatcher::Node(self.new_watcher_inner(kind, stats_collector, None))
            }
            "namespaces" => GenericResourceWatcher::Namespace(self.new_watcher_inner(
                kind,
                stats_collector,
                None,
            )),
            "services" => GenericResourceWatcher::Service(self.new_watcher_inner(
                kind,
                stats_collector,
                namespace,
            )),
            "deployments" => GenericResourceWatcher::Deployment(self.new_watcher_inner(
                kind,
                stats_collector,
                namespace,
            )),
            "pods" => GenericResourceWatcher::Pod(self.new_watcher_inner(
                kind,
                stats_collector,
                namespace,
            )),
            "statefulsets" => GenericResourceWatcher::StatefulSet(self.new_watcher_inner(
                kind,
                stats_collector,
                namespace,
            )),
            "daemonsets" => GenericResourceWatcher::DaemonSet(self.new_watcher_inner(
                kind,
                stats_collector,
                namespace,
            )),
            "replicationcontrollers" => GenericResourceWatcher::ReplicationController(
                self.new_watcher_inner(kind, stats_collector, namespace),
            ),
            "replicasets" => GenericResourceWatcher::ReplicaSet(self.new_watcher_inner(
                kind,
                stats_collector,
                namespace,
            )),
            "v1ingresses" => GenericResourceWatcher::V1Ingress(self.new_watcher_inner(
                kind,
                stats_collector,
                namespace,
            )),
            "v1beta1ingresses" => GenericResourceWatcher::V1beta1Ingress(self.new_watcher_inner(
                kind,
                stats_collector,
                namespace,
            )),
            "extv1beta1ingresses" => GenericResourceWatcher::ExtV1beta1Ingress(
                self.new_watcher_inner(kind, stats_collector, namespace),
            ),
            "routes" => GenericResourceWatcher::Route(self.new_watcher_inner(
                kind,
                stats_collector,
                namespace,
            )),
            _ => return None,
        };

        Some(watcher)
    }
}
