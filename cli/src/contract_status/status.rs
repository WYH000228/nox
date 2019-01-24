/*
 * Copyright 2018 Fluence Labs Limited
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

use derive_getters::Getters;
use failure::Error;
use serde_derive::{Deserialize, Serialize};
use web3::types::Address;

use crate::contract_func::ContractCaller;
use crate::contract_status::app::{get_apps, get_nodes, App};
use crate::contract_status::node::Node;

#[derive(Serialize, Deserialize, Debug, Getters)]
pub struct Status {
    apps: Vec<App>,
    nodes: Vec<Node>,
}

impl Status {
    pub fn new(apps: Vec<App>, nodes: Vec<Node>) -> Status {
        Status { apps, nodes }
    }
}

/// Gets status about Fluence contract from ethereum blockchain.
pub fn get_status(contract_address: Address, eth_url: &str) -> Result<Status, Error> {
    let contract = ContractCaller::new(contract_address, eth_url)?;

    // TODO get more data

    let apps = get_apps(&contract)?;

    let nodes = get_nodes(&contract)?;

    Ok(Status::new(apps, nodes))
}