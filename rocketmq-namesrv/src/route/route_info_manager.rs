/*
 * Licensed to the Apache Software Foundation (ASF) under one or more
 * contributor license agreements.  See the NOTICE file distributed with
 * this work for additional information regarding copyright ownership.
 * The ASF licenses this file to You under the Apache License, Version 2.0
 * (the "License"); you may not use this file except in compliance with
 * the License.  You may obtain a copy of the License at
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
    collections::{HashMap, HashSet},
    time::SystemTime,
};

use rocketmq_common::common::{
    config::TopicConfig, constant::PermName, mix_all, namesrv::namesrv_config::NamesrvConfig,
    topic::TopicValidator,
};
use rocketmq_remoting::protocol::{
    body::{
        broker_body::cluster_info::ClusterInfo,
        topic_info_wrapper::topic_config_wrapper::TopicConfigAndMappingSerializeWrapper,
    },
    namesrv::RegisterBrokerResult,
    route::route_data_view::{BrokerData, QueueData, TopicRouteData},
    static_topic::topic_queue_info::TopicQueueMappingInfo,
    DataVersion,
};
use tracing::{debug, info, warn};

use crate::route_info::broker_addr_info::{BrokerAddrInfo, BrokerLiveInfo};

const DEFAULT_BROKER_CHANNEL_EXPIRED_TIME: i64 = 1000 * 60 * 2;

type TopicQueueTable =
    HashMap<String /* topic */, HashMap<String /* broker name */, QueueData>>;
type BrokerAddrTable = HashMap<String /* brokerName */, BrokerData>;
type ClusterAddrTable = HashMap<String /* clusterName */, HashSet<String /* brokerName */>>;
type BrokerLiveTable = HashMap<BrokerAddrInfo /* brokerAddr */, BrokerLiveInfo>;
type FilterServerTable =
    HashMap<BrokerAddrInfo /* brokerAddr */, Vec<String> /* Filter Server */>;
type TopicQueueMappingInfoTable =
    HashMap<String /* topic */, HashMap<String /* brokerName */, TopicQueueMappingInfo>>;

#[derive(Debug, Clone, Default)]
pub struct RouteInfoManager {
    topic_queue_table: TopicQueueTable,
    broker_addr_table: BrokerAddrTable,
    cluster_addr_table: ClusterAddrTable,
    broker_live_table: BrokerLiveTable,
    filter_server_table: FilterServerTable,
    topic_queue_mapping_info_table: TopicQueueMappingInfoTable,
    namesrv_config: NamesrvConfig,
}

#[allow(private_interfaces)]
impl RouteInfoManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn new_with_config(namesrv_config: NamesrvConfig) -> Self {
        RouteInfoManager {
            topic_queue_table: HashMap::new(),
            broker_addr_table: HashMap::new(),
            cluster_addr_table: HashMap::new(),
            broker_live_table: HashMap::new(),
            filter_server_table: HashMap::new(),
            topic_queue_mapping_info_table: HashMap::new(),
            namesrv_config,
        }
    }
}

//impl register broker
impl RouteInfoManager {
    pub fn register_broker(
        &mut self,
        cluster_name: String,
        broker_addr: String,
        broker_name: String,
        broker_id: i64,
        ha_server_addr: String,
        zone_name: Option<String>,
        _timeout_millis: Option<i64>,
        enable_acting_master: Option<bool>,
        topic_config_serialize_wrapper: TopicConfigAndMappingSerializeWrapper,
        filter_server_list: Vec<String>,
    ) -> Option<RegisterBrokerResult> {
        let mut result = RegisterBrokerResult::default();
        //init or update cluster information
        if !self.cluster_addr_table.contains_key(&cluster_name) {
            self.cluster_addr_table
                .insert(cluster_name.to_string(), HashSet::new());
        }
        self.cluster_addr_table
            .get_mut(&cluster_name)
            .unwrap()
            .insert(broker_name.clone());

        let is_old_version_broker = if let Some(value) = enable_acting_master {
            value
        } else {
            false
        };
        let mut register_first =
            if let Some(broker_data) = self.broker_addr_table.get_mut(&broker_name) {
                broker_data.set_enable_acting_master(is_old_version_broker);
                broker_data.set_zone_name(zone_name.clone());
                false
            } else {
                self.broker_addr_table.insert(
                    broker_name.clone(),
                    BrokerData::new(
                        cluster_name.clone(),
                        broker_name.clone(),
                        HashMap::new(),
                        zone_name,
                    ),
                );
                true
            };
        let broker_data = self.broker_addr_table.get_mut(&broker_name).unwrap();
        let mut prev_min_broker_id = 0i64;
        if !broker_data.broker_addrs().is_empty() {
            prev_min_broker_id = broker_data.broker_addrs().keys().min().copied().unwrap();
        }
        let mut is_min_broker_id_changed = false;
        if broker_id < prev_min_broker_id {
            is_min_broker_id_changed = true;
        }

        //Switch slave to master: first remove <1, IP:PORT> in rocketmq-namesrv, then add <0,
        // IP:PORT> The same IP:PORT must only have one record in brokerAddrTable
        broker_data.remove_broker_by_addr(broker_id, &broker_addr);

        if let Some(old_broker_addr) = broker_data.broker_addrs().get(&broker_id) {
            if old_broker_addr != &broker_addr {
                let addr_info =
                    BrokerAddrInfo::new(cluster_name.clone(), old_broker_addr.to_string());
                if let Some(val) = self.broker_live_table.get(&addr_info) {
                    let old_state_version = val.data_version().state_version();
                    let new_state_version = topic_config_serialize_wrapper
                        .data_version()
                        .as_ref()
                        .unwrap()
                        .state_version();
                    if old_state_version > new_state_version {
                        self.broker_live_table.remove(
                            BrokerAddrInfo::new(cluster_name.clone(), broker_addr.clone()).as_ref(),
                        );
                        return Some(result);
                    }
                }
            }
        }
        let size = if let Some(val) = topic_config_serialize_wrapper.topic_config_table() {
            val.len()
        } else {
            0
        };
        if !broker_data.broker_addrs().contains_key(&broker_id) && size == 1 {
            warn!(
                "Can't register topicConfigWrapper={:?} because broker[{}]={} has not registered.",
                topic_config_serialize_wrapper.topic_config_table(),
                broker_id,
                broker_addr
            );
            return None;
        }

        let old_addr = broker_data
            .broker_addrs_mut()
            .insert(broker_id, broker_addr.clone());

        register_first |= old_addr.is_none();
        let is_master = mix_all::MASTER_ID == broker_id as u64;

        let is_prime_slave = !is_old_version_broker
            && !is_master
            && broker_id == broker_data.broker_addrs().keys().min().copied().unwrap();
        let broker_data = broker_data.clone();
        if is_master || is_prime_slave {
            if let Some(tc_table) = topic_config_serialize_wrapper.topic_config_table() {
                let topic_queue_mapping_info_map =
                    topic_config_serialize_wrapper.topic_queue_mapping_info_map();
                if self.namesrv_config.delete_topic_with_broker_registration
                    && topic_queue_mapping_info_map.is_empty()
                {
                    let old_topic_set = self.topic_set_of_broker_name(&broker_name);
                    let new_topic_set = tc_table
                        .keys()
                        .map(|item| item.to_string())
                        .collect::<HashSet<String>>();
                    let to_delete_topics = new_topic_set
                        .difference(&old_topic_set)
                        .map(|item| item.to_string())
                        .collect::<HashSet<String>>();
                    for to_delete_topic in to_delete_topics {
                        let queue_data_map = self.topic_queue_table.get_mut(&to_delete_topic);
                        if let Some(queue_data) = queue_data_map {
                            let removed_qd = queue_data.remove(&broker_name);
                            if let Some(ref removed_qd_inner) = removed_qd {
                                info!(
                                    "broker[{}] delete topic[{}] queue[{:?}] because of master \
                                     change",
                                    broker_name, to_delete_topic, removed_qd_inner
                                );
                            }
                            if queue_data.is_empty() {
                                self.topic_queue_table.remove(&to_delete_topic);
                            }
                        }
                    }
                }
                let data_version = topic_config_serialize_wrapper
                    .data_version()
                    .as_ref()
                    .unwrap();
                for topic_config in tc_table.values() {
                    let mut config = topic_config.clone();
                    if (register_first
                        || self.is_topic_config_changed(
                            &cluster_name,
                            &broker_addr,
                            data_version,
                            &broker_name,
                            &topic_config.topic_name,
                        ))
                        && is_prime_slave
                        && broker_data.enable_acting_master()
                    {
                        config.perm &= !(PermName::PERM_WRITE as u32);
                    }
                    self.create_and_update_queue_data(&broker_name, config);
                }
                if self.is_broker_topic_config_changed(&cluster_name, &broker_addr, data_version)
                    || register_first
                {
                    for (topic, vtq_info) in topic_queue_mapping_info_map {
                        if !self.topic_queue_mapping_info_table.contains_key(topic) {
                            self.topic_queue_mapping_info_table
                                .insert(topic.to_string(), HashMap::new());
                        }
                        self.topic_queue_mapping_info_table
                            .get_mut(topic)
                            .unwrap()
                            .insert(
                                vtq_info.bname.as_ref().unwrap().to_string(),
                                vtq_info.clone(),
                            );
                    }
                }
            }
        }

        let broker_addr_info = BrokerAddrInfo::new(cluster_name.clone(), broker_addr.clone());

        self.broker_live_table.insert(
            broker_addr_info.clone(),
            BrokerLiveInfo::new(
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .expect("Time went backwards")
                    .as_millis() as i64,
                DEFAULT_BROKER_CHANNEL_EXPIRED_TIME,
                if let Some(data_version) = topic_config_serialize_wrapper.data_version() {
                    data_version.clone()
                } else {
                    DataVersion::default()
                },
                ha_server_addr.clone(),
            ),
        );
        if filter_server_list.is_empty() {
            self.filter_server_table.remove(&broker_addr_info);
        } else {
            self.filter_server_table
                .insert(broker_addr_info, filter_server_list);
        }

        if mix_all::MASTER_ID != broker_id as u64 {
            let master_address = broker_data.broker_addrs().get(&(mix_all::MASTER_ID as i64));
            if let Some(master_addr) = master_address {
                let master_livie_info = self
                    .broker_live_table
                    .get(BrokerAddrInfo::new(cluster_name.clone(), master_addr.clone()).as_ref());
                if let Some(info) = master_livie_info {
                    result.ha_server_addr = info.ha_server_addr().to_string();
                    result.master_addr = info.ha_server_addr().to_string();
                }
            }
        }
        if is_min_broker_id_changed && self.namesrv_config.notify_min_broker_id_changed {
            todo!()
        }
        Some(result)
    }
}

impl RouteInfoManager {
    pub(crate) fn get_all_cluster_info(&self) -> ClusterInfo {
        ClusterInfo::new(
            Some(self.broker_addr_table.clone()),
            Some(self.cluster_addr_table.clone()),
        )
    }

    pub(crate) fn pickup_topic_route_data(&self, topic: &str) -> Option<TopicRouteData> {
        let mut topic_route_data = TopicRouteData {
            order_topic_conf: None,
            broker_datas: Vec::new(),
            queue_datas: Vec::new(),
            filter_server_table: HashMap::new(),
            topic_queue_mapping_by_broker: None,
        };

        let mut found_queue_data = false;
        let mut found_broker_data = false;

        // Acquire read lock

        if let Some(queue_data_map) = self.topic_queue_table.get(topic) {
            topic_route_data.queue_datas = queue_data_map.values().cloned().collect();
            found_queue_data = true;

            let broker_name_set: HashSet<&String> = queue_data_map.keys().collect();

            for broker_name in broker_name_set {
                if let Some(broker_data) = self.broker_addr_table.get(broker_name) {
                    let broker_data_clone = broker_data.clone();
                    topic_route_data.broker_datas.push(broker_data_clone);
                    found_broker_data = true;

                    if !self.filter_server_table.is_empty() {
                        for broker_addr in broker_data.broker_addrs().values() {
                            let broker_addr_info =
                                BrokerAddrInfo::new(broker_data.cluster(), broker_addr.clone());
                            if let Some(filter_server_list) =
                                self.filter_server_table.get(&broker_addr_info)
                            {
                                topic_route_data
                                    .filter_server_table
                                    .insert(broker_addr.clone(), filter_server_list.clone());
                            }
                        }
                    }
                }
            }
        }

        debug!("pickup_topic_route_data {:?} {:?}", topic, topic_route_data);

        if found_broker_data && found_queue_data {
            topic_route_data.topic_queue_mapping_by_broker = Some(
                self.topic_queue_mapping_info_table
                    .get(topic)
                    .cloned()
                    .unwrap_or_default(),
            );

            if !self.namesrv_config.support_acting_master {
                return Some(topic_route_data);
            }

            if topic.starts_with(TopicValidator::SYNC_BROKER_MEMBER_GROUP_PREFIX) {
                return Some(topic_route_data);
            }

            if topic_route_data.broker_datas.is_empty() || topic_route_data.queue_datas.is_empty() {
                return Some(topic_route_data);
            }

            let need_acting_master = topic_route_data.broker_datas.iter().any(|broker_data| {
                !broker_data.broker_addrs().is_empty()
                    && !broker_data
                        .broker_addrs()
                        .contains_key(&(mix_all::MASTER_ID as i64))
            });

            if !need_acting_master {
                return Some(topic_route_data);
            }

            for broker_data in &mut topic_route_data.broker_datas {
                if broker_data.broker_addrs().is_empty()
                    || broker_data
                        .broker_addrs()
                        .contains_key(&(mix_all::MASTER_ID as i64))
                    || !broker_data.enable_acting_master()
                {
                    continue;
                }

                // No master
                for queue_data in &topic_route_data.queue_datas {
                    if queue_data.broker_name() == broker_data.broker_name() {
                        if !PermName::is_writeable(queue_data.perm() as i8) {
                            if let Some(min_broker_id) =
                                broker_data.broker_addrs().keys().cloned().min()
                            {
                                if let Some(acting_master_addr) =
                                    broker_data.broker_addrs_mut().remove(&min_broker_id)
                                {
                                    broker_data
                                        .broker_addrs_mut()
                                        .insert(mix_all::MASTER_ID as i64, acting_master_addr);
                                }
                            }
                        }
                        break;
                    }
                }
            }

            return Some(topic_route_data);
        }

        None
    }
}

impl RouteInfoManager {
    fn topic_set_of_broker_name(&mut self, broker_name: &str) -> HashSet<String> {
        let mut topic_of_broker = HashSet::new();
        for (key, value) in self.topic_queue_table.iter() {
            if value.contains_key(broker_name) {
                topic_of_broker.insert(key.to_string());
            }
        }
        topic_of_broker
    }

    fn is_topic_config_changed(
        &mut self,
        cluster_name: &str,
        broker_addr: &str,
        data_version: &DataVersion,
        broker_name: &str,
        topic: &str,
    ) -> bool {
        let is_change =
            self.is_broker_topic_config_changed(cluster_name, broker_addr, data_version);
        if is_change {
            return true;
        }
        let queue_data_map = self.topic_queue_table.get(topic);
        if let Some(queue_data) = queue_data_map {
            if queue_data.is_empty() {
                return true;
            }
            !queue_data.contains_key(broker_name)
        } else {
            true
        }
    }

    fn is_broker_topic_config_changed(
        &mut self,
        cluster_name: &str,
        broker_addr: &str,
        data_version: &DataVersion,
    ) -> bool {
        let option = self.query_broker_topic_config(cluster_name, broker_addr);
        if let Some(pre) = option {
            if !(pre == data_version) {
                return true;
            }
        }
        false
    }

    fn query_broker_topic_config(
        &mut self,
        cluster_name: &str,
        broker_addr: &str,
    ) -> Option<&DataVersion> {
        let info = BrokerAddrInfo::new(cluster_name.to_string(), broker_addr.to_string());
        let pre = self.broker_live_table.get(info.as_ref());
        if let Some(live_info) = pre {
            return Some(live_info.data_version());
        }
        None
    }

    fn create_and_update_queue_data(&mut self, broker_name: &str, topic_config: TopicConfig) {
        let queue_data = QueueData::new(
            broker_name.to_string(),
            topic_config.write_queue_nums,
            topic_config.read_queue_nums,
            topic_config.perm,
            topic_config.topic_sys_flag,
        );

        let queue_data_map = self.topic_queue_table.get_mut(&topic_config.topic_name);
        if let Some(queue_data_map_inner) = queue_data_map {
            let existed_qd = queue_data_map_inner.get(broker_name);
            if existed_qd.is_none() {
                queue_data_map_inner.insert(broker_name.to_string(), queue_data);
            } else {
                let unwrap = existed_qd.unwrap();
                if unwrap != &queue_data {
                    info!(
                        "topic changed, {} OLD: {:?} NEW: {:?}",
                        &topic_config.topic_name, unwrap, queue_data
                    );
                    queue_data_map_inner.insert(broker_name.to_string(), queue_data);
                }
            }
        } else {
            let mut queue_data_map_inner = HashMap::new();
            info!(
                "new topic registered, {} {:?}",
                &topic_config.topic_name, &queue_data
            );
            queue_data_map_inner.insert(broker_name.to_string(), queue_data);
            self.topic_queue_table
                .insert(topic_config.topic_name.clone(), queue_data_map_inner);
        }
    }
}