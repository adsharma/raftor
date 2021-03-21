use actix::prelude::*;
use actix_raft::{
    admin::{InitWithConfig, ProposeConfigChange},
    messages::*,
    NodeId,
};

use std::time::{Duration, Instant};
use std::sync::{Arc, RwLock};
use serde::{Serialize, Deserialize};
use tokio::timer::Delay;
use crate::network::{Network, remote::SendRemoteMessage, GetCurrentLeader, GetNodeById, HandlerRegistry};
use crate::raft::{
    storage::{MemoryStorageData, MemoryStorageError, MemoryStorageResponse},
    RaftBuilder, MemRaft,
};
use crate::hash_ring::RingType;
use crate::server::Server;

type ClientResponseHandler = Result<
    ClientPayloadResponse<MemoryStorageResponse>,
    ClientError<MemoryStorageData, MemoryStorageResponse, MemoryStorageError>,
>;

pub type Payload = ClientPayload<MemoryStorageData, MemoryStorageResponse, MemoryStorageError>;

pub struct RaftClient {
    id: NodeId,
    ring: RingType,
    raft: Option<Addr<MemRaft>>,
    registry: Arc<RwLock<HandlerRegistry>>,
    net: Option<Addr<Network>>,
}

impl Actor for RaftClient {
    type Context = Context<Self>;

    fn started(&mut self, _ctx: &mut Context<Self>) {}
}

impl RaftClient {
    pub fn new(id: NodeId, ring: RingType, registry: Arc<RwLock<HandlerRegistry>>) -> RaftClient {
        RaftClient {
            id: id,
            ring: ring,
            raft: None,
            registry: registry,
            net: None,
        }

    }

    fn register_handlers(&mut self, raft: Addr<MemRaft>, client: Addr<Self>) {
        let mut registry = self.registry.write().unwrap();

        registry.register::<AppendEntriesRequest<MemoryStorageData>, _>(raft.clone());
        registry.register::<VoteRequest, _>(raft.clone());
        registry.register::<InstallSnapshotRequest, _>(raft.clone());
        registry.register::<ChangeRaftClusterConfig, _>(client.clone());
        registry.register::<ClientPayload<MemoryStorageData, MemoryStorageResponse, MemoryStorageError>, _>(raft.clone());
    }
}

#[derive(Message)]
pub struct InitRaft {
    pub nodes: Vec<NodeId>,
    pub net: Addr<Network>,
    pub server: Addr<Server>,
    pub join_mode: bool,
}

#[derive(Message)]
pub struct AddNode(pub NodeId);

#[derive(Serialize, Deserialize ,Message, Clone)]
pub struct ChangeRaftClusterConfig(pub Vec<NodeId>, pub Vec<NodeId>);

impl Handler<ChangeRaftClusterConfig> for RaftClient {
    type Result = ();

    fn handle(&mut self, msg: ChangeRaftClusterConfig, ctx: &mut Context<Self>) {
        let nodes_to_add = msg.0.clone();
        let nodes_to_remove = msg.1.clone();

        let payload = ProposeConfigChange::new(nodes_to_add.clone(), nodes_to_remove.clone());

        ctx.spawn(
            fut::wrap_future::<_, Self>(self.net.as_ref().unwrap().send(GetCurrentLeader))
                .map_err(|err, _, _| panic!(err))
                .and_then(move |res, act, _ctx| {
                    let leader = res.unwrap();

                    if leader == act.id {
                        if let Some(ref raft) = act.raft {
                            println!(" ------------- About to propose config change");
                            return fut::Either::A(
                                fut::wrap_future::<_, Self>(raft.send(payload))
                                    .map_err(|err, _, _| panic!(err))
                                    .and_then(move |_res, _act, ctx| {
                                        for id in nodes_to_add.iter() {
                                            ctx.notify(AddNode(*id));
                                        }

                                        fut::ok(())
                                    }),
                            );
                        }
                    }

                    fut::Either::B(
                        fut::wrap_future::<_, Self>(act.net.as_ref().unwrap().send(GetNodeById(leader)))
                            .map_err(move |_err, _, _| panic!("Node {} not found", leader))
                            .and_then(move |node, _act, _ctx| {
                                println!("-------------- Sending remote proposal to leader");
                                fut::wrap_future::<_, Self>(
                                    node.unwrap().send(SendRemoteMessage(msg.clone())),
                                )
                                    .map_err(|err, _, _| println!("Error {:?}", err))
                                    .and_then(|_res, _act, _ctx| {
                                        fut::ok(())
                                    })
                            }),
                    )
                }),
        );
    }
}

#[derive(Message)]
pub struct RemoveNode(pub NodeId);

impl Handler<AddNode> for RaftClient {
    type Result = ();

    fn handle(&mut self, msg: AddNode, ctx: &mut Context<Self>) {
        let payload = add_node(msg.0);
        ctx.notify(ClientRequest(payload));
    }
}

impl Handler<RemoveNode> for RaftClient {
    type Result = ();

    fn handle(&mut self, msg: RemoveNode, ctx: &mut Context<Self>) {
        let payload = remove_node(msg.0);
        ctx.notify(ClientRequest(payload));
        ctx.notify(ChangeRaftClusterConfig(vec![], vec![msg.0]));
    }
}

impl Handler<InitRaft> for RaftClient {
    type Result = ();

    fn handle(&mut self, msg: InitRaft, ctx: &mut Context<Self>) {
        let nodes = msg.nodes;
        self.net = Some(msg.net);
        let server = msg.server;

        let nodes = if msg.join_mode {
            vec![self.id]
        } else {
            nodes.clone()
        };

        let raft =
            RaftBuilder::new(self.id, nodes.clone(), self.net.as_ref().unwrap().clone(), self.ring.clone(), server);
        self.register_handlers(raft.clone(), ctx.address().clone());
        self.raft = Some(raft);

        if msg.join_mode {
            return ();
        }

        fut::wrap_future::<_, Self>(Delay::new(Instant::now() + Duration::from_secs(5)))
            .map_err(|_, _, _| ())
            .and_then(move |_, act, _ctx| {
                fut::wrap_future::<_, Self>(
                    act.raft
                        .as_ref()
                        .unwrap()
                        .send(InitWithConfig::new(nodes.clone())),
                )
                    .map_err(|err, _, _| panic!(err))
                    .and_then(|_, _, _| {
                        println!("Inited with config!");
                        fut::wrap_future::<_, Self>(Delay::new(
                            Instant::now() + Duration::from_secs(5),
                        ))
                    })
                    .map_err(|_, _, _| ())
                    .and_then(|_, act, ctx| {
                        let payload = add_node(act.id);
                        ctx.notify(ClientRequest(payload));
                        fut::ok(())
                    })
            })
            .spawn(ctx);
    }
}

pub struct ClientRequest(pub MemoryStorageData);

impl Message for ClientRequest {
    type Result = ();
}

impl Handler<ClientRequest> for RaftClient {
    type Result = ();

    fn handle(&mut self, msg: ClientRequest, ctx: &mut Context<Self>) {
        let entry = EntryNormal {
            data: msg.0.clone(),
        };

        let payload = Payload::new(entry, ResponseMode::Applied);

        ctx.spawn(
            fut::wrap_future::<_, Self>(self.net.as_ref().unwrap().send(GetCurrentLeader))
                .map_err(|err, _, _| panic!(err))
                .and_then(move |res, act, _ctx| {
                    let leader = res.unwrap();

                    if leader == act.id {
                        if let Some(ref raft) = act.raft {
                            return fut::Either::A(
                                fut::wrap_future::<_, Self>(raft.send(payload))
                                    .map_err(|err, _, _| panic!(err))
                                    .and_then(|res, _act, ctx| {
                                        fut::ok(handle_client_response(res, ctx, msg))
                                    }),
                            );
                        }
                    }

                    fut::Either::B(
                        fut::wrap_future::<_, Self>(act.net.as_ref().unwrap().send(GetNodeById(leader)))
                            .map_err(move |_err, _, _| panic!("Node {} not found", leader))
                            .and_then(move |node, _act, _ctx| {
                                println!("About to do something with node {}", leader);
                                fut::wrap_future::<_, Self>(
                                    node.unwrap().send(SendRemoteMessage(payload)),
                                )
                                    .map_err(|err, _, _| println!("Error {:?}", err))
                                    .and_then(|res, _act, ctx| {
                                        fut::ok(handle_client_response(res, ctx, msg))
                                    })
                            }),
                    )
                }),
        );
    }
}

fn add_node(id: NodeId) -> MemoryStorageData {
    MemoryStorageData::Add(id)
}

fn remove_node(id: NodeId) -> MemoryStorageData {
    MemoryStorageData::Remove(id)
}

fn handle_client_response(
    res: ClientResponseHandler,
    ctx: &mut Context<RaftClient>,
    msg: ClientRequest,
) {
    match res {
        Ok(_) => (),
        Err(err) => match err {
            ClientError::Internal => {
                println!("TEST: resending client request.");
                ctx.notify(msg);
            }
            ClientError::Application(err) => {
                println!(
                    "Unexpected application error from client request: {:?}",
                    err
                );
            }
            ClientError::ForwardToLeader { .. } => {
                println!("TEST: received ForwardToLeader error. Updating leader and forwarding.");
                ctx.notify(msg);
            }
        },
    }
}
