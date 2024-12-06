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

#[cfg_attr(feature = "nightly-features", feature(error_generic_member_access))]
#[cfg(feature = "nightly-features")]
use std::backtrace::Backtrace;
use std::{
    fmt, io,
    num::TryFromIntError,
    result,
    str::Utf8Error,
    string::FromUtf8Error,
    sync::{Arc, PoisonError},
};

use serde::{Deserialize, Serialize};
use tansu_kafka_sans_io::{
    broker_registration_request::Listener,
    create_topics_request::{CreatableReplicaAssignment, CreatableTopic},
    ErrorCode,
};
use thiserror::Error;
use tracing_subscriber::filter::ParseError;
use url::Url;
use uuid::Uuid;

pub mod broker;
pub mod coordinator;

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
    ClientRpc(#[from] tarpc::client::RpcError),
    Custom(String),
    EmptyCoordinatorWrapper,
    EmptyJoinGroupRequestProtocol,
    ExpectedJoinGroupRequestProtocol(&'static str),
    Io(Arc<io::Error>),
    Json(#[from] serde_json::Error),
    KafkaProtocol {
        #[from]
        source: tansu_kafka_sans_io::Error,
        #[cfg(feature = "nightly-features")]
        backtrace: Backtrace,
    },
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
