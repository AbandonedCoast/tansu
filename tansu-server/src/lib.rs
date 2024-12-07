// Copyright ⓒ 2024 Peter Morgan <peter.james.morgan@gmail.com>
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use std::{
    fmt, io,
    num::TryFromIntError,
    result,
    str::{FromStr, Utf8Error},
    string::FromUtf8Error,
    sync::{Arc, PoisonError},
};

use serde::{Deserialize, Serialize};
use tansu_kafka_sans_io::{
    broker_registration_request::Listener,
    create_topics_request::{CreatableReplicaAssignment, CreatableTopic},
    fetch_request::FetchTopic,
    list_partition_reassignments_request::ListPartitionReassignmentsTopics,
    produce_request::TopicProduceData,
    ErrorCode,
};
use thiserror::Error;
use tracing_subscriber::filter::ParseError;
use url::Url;
use uuid::Uuid;

pub mod broker;
pub mod coordinator;

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum TopicId {
    Name(String),
    Id(Uuid),
}

impl From<&FetchTopic> for TopicId {
    fn from(value: &FetchTopic) -> Self {
        if let Some(ref topic) = value.topic {
            Self::Name(topic.to_string())
        } else if let Some(topic_id) = value.topic_id {
            Self::Id(Uuid::from_bytes(topic_id))
        } else {
            todo!()
        }
    }
}

impl From<&ListPartitionReassignmentsTopics> for TopicId {
    fn from(value: &ListPartitionReassignmentsTopics) -> Self {
        Self::Name(value.name.to_owned())
    }
}

impl From<&TopicProduceData> for TopicId {
    fn from(value: &TopicProduceData) -> Self {
        Self::Name(value.name.to_owned())
    }
}

impl From<&CreatableTopic> for TopicId {
    fn from(value: &CreatableTopic) -> Self {
        Self::Name(value.name.to_owned())
    }
}

impl From<&str> for TopicId {
    fn from(value: &str) -> Self {
        Self::Name(value.to_string())
    }
}

impl From<String> for TopicId {
    fn from(value: String) -> Self {
        Self::Name(value)
    }
}

impl FromStr for TopicId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::Name(s.to_string()))
    }
}

impl From<[u8; 16]> for TopicId {
    fn from(value: [u8; 16]) -> Self {
        Self::Id(Uuid::from_bytes(value))
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct TopicDetail {
    id: [u8; 16],
    creatable_topic: CreatableTopic,
}

impl TopicDetail {
    pub fn id(&self) -> [u8; 16] {
        self.id
    }

    pub fn name(&self) -> &str {
        self.creatable_topic.name.as_str()
    }

    pub fn replica_assignments(&self) -> Option<&[CreatableReplicaAssignment]> {
        self.creatable_topic.assignments.as_deref()
    }
}

#[derive(
    Copy, Clone, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct NodeDetail {
    port: u16,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct BrokerDetail {
    incarnation_id: Uuid,
    listeners: Option<Vec<Listener>>,
    rack: Option<String>,
}

#[derive(Error, Debug)]
pub enum Error {
    Api(ErrorCode),
    Custom(String),
    EmptyCoordinatorWrapper,
    EmptyJoinGroupRequestProtocol,
    ExpectedJoinGroupRequestProtocol(&'static str),
    Io(Arc<io::Error>),
    Json(#[from] serde_json::Error),
    KafkaProtocol(#[from] tansu_kafka_sans_io::Error),
    Message(String),
    Model(#[from] tansu_kafka_model::Error),
    ObjectStore(#[from] object_store::Error),
    ParseFilter(#[from] ParseError),
    ParseInt(#[from] std::num::ParseIntError),
    Poison,
    Pool(#[from] deadpool_postgres::PoolError),
    Storage(#[from] tansu_storage::Error),
    StringUtf8(#[from] FromUtf8Error),
    TokioPostgres(#[from] tokio_postgres::error::Error),
    TryFromInt(#[from] TryFromIntError),
    UnsupportedStorageUrl(Url),
    Url(#[from] url::ParseError),
    Utf8(#[from] Utf8Error),
    Uuid(#[from] uuid::Error),
}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(Arc::new(value))
    }
}

impl<T> From<PoisonError<T>> for Error {
    fn from(_value: PoisonError<T>) -> Self {
        Self::Poison
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(msg) => write!(f, "{}", msg),
            error => write!(f, "{:?}", error),
        }
    }
}

pub type Result<T, E = Error> = result::Result<T, E>;
