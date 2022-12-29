#![allow(dead_code)] // TODO: remove
#![allow(clippy::mutable_key_type)] // `HelperIdentity` cannot be modified

pub mod http;
pub mod query;

mod error;

pub use error::Error;

use crate::{
    helpers::HelperIdentity,
    protocol::{QueryId, Step},
};
use async_trait::async_trait;
use futures::Stream;

// /// All of the `TransportCommand`s should have a request/response model, and this trait ensures a
// /// way to respond
// pub trait TransportCommandData {
//     /// type of data to respond with
//     type RespData;
//     /// name of the command
//     fn name() -> &'static str;
//     /// respond to a `TransportCommand`
//     /// # Errors
//     /// if the requester stops waiting for a response
//     fn respond(self, query_id: QueryId, data: Self::RespData) -> Result<(), Error>;
// }

// #[derive(Debug)]
// pub struct CreateQueryData {
//     pub field_type: FieldType,
//     pub helper_positions: [HelperIdentity; 3],
//     callback: oneshot::Sender<(QueryId, <Self as TransportCommandData>::RespData)>,
// }

// impl CreateQueryData {
//     #[must_use]
//     pub fn new(
//         field_type: FieldType,
//         helper_positions: [HelperIdentity; 3],
//         callback: oneshot::Sender<(QueryId, <Self as TransportCommandData>::RespData)>,
//     ) -> Self {
//         CreateQueryData {
//             field_type,
//             helper_positions,
//             callback,
//         }
//     }
// }

// impl TransportCommandData for CreateQueryData {
//     type RespData = HelperIdentity;
//     fn name() -> &'static str {
//         "CreateQuery"
//     }

//     fn respond(self, query_id: QueryId, data: Self::RespData) -> Result<(), Error> {
//         self.callback
//             .send((query_id, data))
//             .map_err(|_| Error::CallbackFailed {
//                 command_name: Self::name(),
//                 query_id,
//             })
//     }
// }

// #[derive(Debug)]
// pub struct PrepareQueryData {
//     pub query_id: QueryId,
//     pub field_type: FieldType,
//     pub helper_positions: [HelperIdentity; 3],
//     pub helpers_to_roles: HashMap<HelperIdentity, Role>,
//     callback: oneshot::Sender<()>,
// }

// impl PrepareQueryData {
//     #[must_use]
//     pub fn new(
//         query_id: QueryId,
//         field_type: FieldType,
//         helper_positions: [HelperIdentity; 3],
//         helpers_to_roles: HashMap<HelperIdentity, Role>,
//         callback: oneshot::Sender<()>,
//     ) -> Self {
//         PrepareQueryData {
//             query_id,
//             field_type,
//             helper_positions,
//             helpers_to_roles,
//             callback,
//         }
//     }
// }

// impl TransportCommandData for PrepareQueryData {
//     type RespData = ();
//     fn name() -> &'static str {
//         "PrepareQuery"
//     }
//     fn respond(self, query_id: QueryId, _: Self::RespData) -> Result<(), Error> {
//         self.callback.send(()).map_err(|_| Error::CallbackFailed {
//             command_name: Self::name(),
//             query_id,
//         })
//     }
// }

// #[derive(Debug)]
// pub struct StartMulData {
//     pub query_id: QueryId,
//     pub data_stream: ByteArrStream,
//     callback: oneshot::Sender<()>,
// }

// impl StartMulData {
//     #[must_use]
//     pub fn new(
//         query_id: QueryId,
//         data_stream: ByteArrStream,
//         callback: oneshot::Sender<()>,
//     ) -> Self {
//         StartMulData {
//             query_id,
//             data_stream,
//             callback,
//         }
//     }
// }

// impl TransportCommandData for StartMulData {
//     type RespData = ();
//     fn name() -> &'static str {
//         "StartMul"
//     }
//     fn respond(self, query_id: QueryId, _: Self::RespData) -> Result<(), Error> {
//         self.callback.send(()).map_err(|_| Error::CallbackFailed {
//             command_name: Self::name(),
//             query_id,
//         })
//     }
// }

// #[derive(Debug)]
// pub struct MulData {
//     pub query_id: QueryId,
//     pub field_type: String,
//     pub data_stream: ByteArrStream,
// }

// impl MulData {
//     #[must_use]
//     pub fn new(query_id: QueryId, field_type: String, data_stream: ByteArrStream) -> Self {
//         Self {
//             query_id,
//             field_type,
//             data_stream,
//         }
//     }
// }

// impl TransportCommandData for MulData {
//     type RespData = ();
//     fn name() -> &'static str {
//         "Mul"
//     }
//     fn respond(self, _: QueryId, _: Self::RespData) -> Result<(), Error> {
//         Ok(())
//     }
// }

// #[derive(Debug)]
// pub struct StepData {
//     pub query_id: QueryId,
//     pub message_chunks: MessageChunks,
//     // to be removed when we switch to the streaming implementation
//     pub offset: u32,
// }

// impl StepData {
//     #[must_use]
//     pub fn new(query_id: QueryId, message_chunks: MessageChunks, offset: u32) -> Self {
//         Self {
//             query_id,
//             message_chunks,
//             offset,
//         }
//     }
// }

// impl TransportCommandData for StepData {
//     type RespData = ();
//     fn name() -> &'static str {
//         "Step"
//     }
//     fn respond(self, _: QueryId, _: Self::RespData) -> Result<(), Error> {
//         Ok(())
//     }
// }

// #[derive(Debug)]
// pub enum TransportCommand {
//     // `Administration` Commands

//     // Helper which receives this command becomes the de facto leader of the query setup. It will:
//     // * generate `query_id`
//     // * assign roles to each helper, generating a role mapping `helper id` -> `role`
//     // * store `query_id` -> (`context_type`, `field_type`, `secret share mapping`, `role mapping`)
//     // * inform other helpers of new `query_id` and associated data
//     // * respond with `query_id` and helper which should receive `Start*` command
//     CreateQuery(CreateQueryData),

//     // Helper which receives this message is a follower of the query setup. It will receive this
//     // message from the leader, who has received the `CreateQuery` command. It will:
//     // * store `query_id` -> (`context_type`, `field_type`, `secret share mapping`, `role mapping`)
//     // * respond with ack
//     PrepareQuery(PrepareQueryData),

//     // Helper which receives this message is the leader of the mul protocol, as chosen by the leader
//     // of the `CreateQuery` command. It will:
//     // * retrieve (`context_type`, `field_type`, `secret share mapping`, `role mapping`)
//     // * assign `Transport` using `secret share mapping` and `role mapping`
//     // * break apart incoming data into 3 different streams, 1 for each helper
//     // * send 2 of the streams to other helpers
//     // * run the protocol using final stream of data, `context_type`, `field_type`
//     StartMul(StartMulData),

//     // Helper which receives this message is a follower of the mul protocol. It will:
//     // * retrieve (`context_type`, `field_type`, `secret share mapping`, `role mapping`)
//     // * assign `Transport` using `secret share mapping` and `role mapping`
//     // * run the protocol using incoming stream of data, `context_type`, `field_type`
//     Mul(MulData),

//     // `Query` Commands

//     // `MessageChunks` to be sent over the network
//     Step(StepData),
// }

#[derive(Debug)]
pub enum TransportCommand {
    // `Administration` Commands
    Query(query::QueryCommand),

    // `Query` Commands
    /// Query/step data received from a helper peer.
    /// TODO: this is really bad for performance, once we have channel per step all the way
    /// from gateway to network, this definition should be (QueryId, Step, Stream<Item = Vec<u8>>) instead
    StepData {
        query_id: QueryId,
        step: Step,
        payload: Vec<u8>,
        offset: u32,
    },
}

impl TransportCommand {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Query(query_command) => query_command.name(),
            Self::StepData { .. } => "StepData",
        }
    }

    pub fn query_id(&self) -> Option<QueryId> {
        match self {
            Self::Query(query_command) => query_command.query_id(),
            Self::StepData { query_id, .. } => Some(*query_id),
        }
    }
}

/// Users of a [`Transport`] must subscribe to a specific type of command, and so must pass this
/// type as argument to the `subscribe` function
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum SubscriptionType {
    /// Commands for managing queries
    QueryManagement,
    /// Commands intended for a running query
    Query(QueryId),
}

impl From<&TransportCommand> for SubscriptionType {
    fn from(value: &TransportCommand) -> Self {
        match value {
            TransportCommand::Query(_) => SubscriptionType::QueryManagement,
            TransportCommand::StepData { query_id, .. } => SubscriptionType::Query(*query_id),
        }
    }
}

/// The source of the command, i.e. where it came from. Some may arrive from helper peers, others
/// may come directly from the clients
#[derive(Debug)]
pub enum CommandOrigin {
    Helper(HelperIdentity),
    Other,
}

/// Wrapper around `TransportCommand` that indicates the origin of it.
#[derive(Debug)]
pub struct CommandEnvelope {
    pub origin: CommandOrigin,
    pub payload: TransportCommand,
}

/// Represents the transport layer of the IPA network. Allows layers above to subscribe for events
/// arriving from helper peers or other parties (clients) and also reliably deliver messages using
/// `send` method.
#[async_trait]
pub trait Transport: Send + Sync + 'static {
    type CommandStream: Stream<Item = CommandEnvelope> + Send + Unpin;

    /// Returns the identity of the helper that runs this transport
    fn identity(&self) -> HelperIdentity;

    /// To be called by an entity which will handle the events as indicated by the
    /// [`SubscriptionType`]. There should be only 1 subscriber per type.
    /// # Panics
    /// May panic if attempt to subscribe to the same [`SubscriptionType`] twice
    async fn subscribe(&self, subscription: SubscriptionType) -> Self::CommandStream;

    /// To be called when an entity wants to send commands to the `Transport`.
    async fn send<C: Send + Into<TransportCommand>>(
        &self,
        destination: &HelperIdentity,
        command: C,
    ) -> Result<(), Error>;
}
