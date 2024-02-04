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

use std::path::PathBuf;

use clap::Parser;
use rocketmq_broker::{broker_controller::BrokerController, command::Args};
use rocketmq_common::{
    common::broker::broker_config::BrokerConfig, EnvUtils::EnvUtils, ParseConfigFile,
};
use rocketmq_rust::rocketmq;
use rocketmq_store::config::message_store_config::MessageStoreConfig;
use tracing::info;

#[rocketmq::main]
async fn main() -> anyhow::Result<()> {
    rocketmq_common::log::init_logger();
    let controller = create_broker_controller()?;
    start_broker_controller(controller)?;
    Ok(())
}

fn create_broker_controller() -> anyhow::Result<BrokerController> {
    let args = Args::parse();
    let home = EnvUtils::get_rocketmq_home();
    let (broker_config, message_store_config) = if let Some(ref config_file) = args.config_file {
        let config_file = PathBuf::from(config_file);
        (
            ParseConfigFile::parse_config_file::<BrokerConfig>(config_file.clone())?,
            ParseConfigFile::parse_config_file::<MessageStoreConfig>(config_file.clone())?,
        )
    } else {
        let path_buf = PathBuf::from(home.as_str())
            .join("conf")
            .join("broker.toml");
        (
            ParseConfigFile::parse_config_file::<BrokerConfig>(path_buf.clone())?,
            ParseConfigFile::parse_config_file::<MessageStoreConfig>(path_buf)?,
        )
    };
    info!("Rocketmq(Rust) home: {}", home);
    Ok(BrokerController::new(broker_config, message_store_config))
}

fn start_broker_controller(broker_controller: BrokerController) -> anyhow::Result<()> {
    let mut broker_controller = broker_controller;
    broker_controller.start();
    Ok(())
}