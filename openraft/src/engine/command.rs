use std::fmt::Debug;

use tokio::sync::oneshot;

use crate::core::sm;
use crate::engine::CommandKind;
use crate::error::Infallible;
use crate::error::InitializeError;
use crate::error::InstallSnapshotError;
use crate::progress::entry::ProgressEntry;
use crate::progress::Inflight;
use crate::raft::AppendEntriesResponse;
use crate::raft::InstallSnapshotResponse;
use crate::raft::VoteRequest;
use crate::raft::VoteResponse;
use crate::LeaderId;
use crate::LogId;
use crate::Node;
use crate::NodeId;
use crate::RaftTypeConfig;
use crate::Vote;

/// Commands to send to `RaftRuntime` to execute, to update the application state.
#[derive(Debug)]
pub(crate) enum Command<C>
where C: RaftTypeConfig
{
    /// Becomes a leader, i.e., its `vote` is granted by a quorum.
    /// The runtime initializes leader data when receives this command.
    BecomeLeader,

    /// No longer a leader. Clean up leader's data.
    QuitLeader,

    /// Append one entry.
    AppendEntry { entry: C::Entry },

    /// Append a `range` of entries.
    AppendInputEntries { entries: Vec<C::Entry> },

    /// Replicate the committed log id to other nodes
    ReplicateCommitted { committed: Option<LogId<C::NodeId>> },

    /// Commit log entries that are already persisted in the store, upto `upto`, inclusive.
    ///
    /// To `commit` logs, [`RaftLogStorage::save_committed()`] is called. And then committed logs
    /// will be applied to the state machine by calling [`RaftStateMachine::apply()`].
    ///
    /// And if it is leader, send applied result to the client that proposed the entry.
    ///
    /// [`RaftLogStorage::save_committed()`]: crate::storage::RaftLogStorage::save_committed
    /// [`RaftStateMachine::apply()`]: crate::storage::RaftStateMachine::apply
    Commit {
        // TODO: remove it when sm::Command to apply is generated by Engine
        seq: sm::CommandSeq,
        // TODO: pass the log id list or entries?
        already_committed: Option<LogId<C::NodeId>>,
        upto: LogId<C::NodeId>,
    },

    /// Replicate log entries or snapshot to a target.
    Replicate {
        target: C::NodeId,
        req: Inflight<C::NodeId>,
    },

    /// Membership config changed, need to update replication streams.
    /// The Runtime has to close all old replications and start new ones.
    /// Because a replication stream should only report state for one membership config.
    /// When membership config changes, the membership log id stored in ReplicationCore has to be
    /// updated.
    RebuildReplicationStreams {
        /// Targets to replicate to.
        targets: Vec<(C::NodeId, ProgressEntry<C::NodeId>)>,
    },

    /// Save vote to storage
    SaveVote { vote: Vote<C::NodeId> },

    /// Send vote to all other members
    SendVote { vote_req: VoteRequest<C::NodeId> },

    /// Purge log from the beginning to `upto`, inclusive.
    PurgeLog { upto: LogId<C::NodeId> },

    /// Delete logs that conflict with the leader from a follower/learner since log id `since`,
    /// inclusive.
    DeleteConflictLog { since: LogId<C::NodeId> },

    // TODO(1): current it is only used to replace BuildSnapshot, InstallSnapshot, CancelSnapshot.
    /// A command send to state machine worker [`sm::Worker`].
    ///
    /// The runtime(`RaftCore`) will just forward this command to [`sm::Worker`].
    /// The response will be sent back in a `RaftMsg::StateMachine` message to `RaftCore`.
    StateMachine { command: sm::Command<C> },

    /// Send result to caller
    Respond {
        when: Option<Condition<C::NodeId>>,
        resp: Respond<C::NodeId, C::Node>,
    },
}

impl<C> From<sm::Command<C>> for Command<C>
where C: RaftTypeConfig
{
    fn from(cmd: sm::Command<C>) -> Self {
        Self::StateMachine { command: cmd }
    }
}

/// For unit testing
impl<C> PartialEq for Command<C>
where
    C: RaftTypeConfig,
    C::Entry: PartialEq,
{
    #[rustfmt::skip]
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Command::BecomeLeader,                            Command::BecomeLeader)                                                          => true,
            (Command::QuitLeader,                              Command::QuitLeader)                                                            => true,
            (Command::AppendEntry { entry },                   Command::AppendEntry { entry: b }, )                                            => entry == b,
            (Command::AppendInputEntries { entries },          Command::AppendInputEntries { entries: b }, )                                   => entries == b,
            (Command::ReplicateCommitted { committed },        Command::ReplicateCommitted { committed: b }, )                                 => committed == b,
            (Command::Commit { seq, already_committed, upto, }, Command::Commit { seq: b_seq, already_committed: b_committed, upto: b_upto, }, ) => seq == b_seq && already_committed == b_committed && upto == b_upto,
            (Command::Replicate { target, req },               Command::Replicate { target: b_target, req: other_req, }, )                     => target == b_target && req == other_req,
            (Command::RebuildReplicationStreams { targets },   Command::RebuildReplicationStreams { targets: b }, )                            => targets == b,
            (Command::SaveVote { vote },                       Command::SaveVote { vote: b })                                                  => vote == b,
            (Command::SendVote { vote_req },                   Command::SendVote { vote_req: b }, )                                            => vote_req == b,
            (Command::PurgeLog { upto },                       Command::PurgeLog { upto: b })                                                  => upto == b,
            (Command::DeleteConflictLog { since },             Command::DeleteConflictLog { since: b }, )                                      => since == b,
            (Command::Respond { when, resp: send },            Command::Respond { when: b_when, resp: b })                                     => send == b && when == b_when,
            (Command::StateMachine { command },                Command::StateMachine { command: b })                                           => command == b,
            _ => false,
        }
    }
}

impl<C> Command<C>
where C: RaftTypeConfig
{
    #[allow(dead_code)]
    #[rustfmt::skip]
    pub(crate) fn kind(&self) -> CommandKind {
        match self {
            Command::BecomeLeader                     => CommandKind::Main,
            Command::QuitLeader                       => CommandKind::Main,
            Command::RebuildReplicationStreams { .. } => CommandKind::Main,
            Command::Respond { .. }                   => CommandKind::Main,

            Command::AppendEntry { .. }               => CommandKind::Log,
            Command::AppendInputEntries { .. }        => CommandKind::Log,
            Command::SaveVote { .. }                  => CommandKind::Log,
            Command::PurgeLog { .. }                  => CommandKind::Log,
            Command::DeleteConflictLog { .. }         => CommandKind::Log,

            Command::ReplicateCommitted { .. }        => CommandKind::Network,
            Command::Replicate { .. }                 => CommandKind::Network,
            Command::SendVote { .. }                  => CommandKind::Network,

            Command::StateMachine { .. }              => CommandKind::StateMachine,
            // Apply is firstly handled by RaftCore, then forwarded to state machine worker.
            // TODO: Apply also write `committed` to log-store, which should be run in CommandKind::Log
            Command::Commit { .. }                     => CommandKind::Main,
        }
    }

    /// Return the condition the command waits for if any.
    #[allow(dead_code)]
    #[rustfmt::skip]
    pub(crate) fn condition(&self) -> Option<&Condition<C::NodeId>> {
        match self {
            Command::BecomeLeader                     => None,
            Command::QuitLeader                       => None,
            Command::AppendEntry { .. }               => None,
            Command::AppendInputEntries { .. }        => None,
            Command::ReplicateCommitted { .. }        => None,
            // TODO: Apply also write `committed` to log-store, which should be run in CommandKind::Log
            Command::Commit { .. }                     => None,
            Command::Replicate { .. }                 => None,
            Command::RebuildReplicationStreams { .. } => None,
            Command::SaveVote { .. }                  => None,
            Command::SendVote { .. }                  => None,
            Command::PurgeLog { .. }                  => None,
            Command::DeleteConflictLog { .. }         => None,
            Command::Respond { when, .. }             => when.as_ref(),
            // TODO(1): 
            Command::StateMachine { .. }              => None,
        }
    }
}

/// A condition to wait for before running a command.
#[derive(Debug, Clone, Copy)]
#[derive(PartialEq, Eq)]
pub(crate) enum Condition<NID>
where NID: NodeId
{
    /// Wait until the log is flushed to the disk.
    ///
    /// In raft, a log io can be uniquely identified by `(leader_id, log_id)`, not `log_id`.
    /// A same log id can be written multiple times by different leaders.
    #[allow(dead_code)]
    LogFlushed {
        leader: LeaderId<NID>,
        log_id: Option<LogId<NID>>,
    },

    /// Wait until the log is applied to the state machine.
    #[allow(dead_code)]
    Applied { log_id: Option<LogId<NID>> },

    /// Wait until a [`sm::Worker`] command is finished.
    #[allow(dead_code)]
    StateMachineCommand { command_seq: sm::CommandSeq },
}

/// A command to send return value to the caller via a `oneshot::Sender`.
#[derive(Debug)]
#[derive(PartialEq, Eq)]
#[derive(derive_more::From)]
pub(crate) enum Respond<NID, N>
where
    NID: NodeId,
    N: Node,
{
    Vote(ValueSender<Result<VoteResponse<NID>, Infallible>>),
    AppendEntries(ValueSender<Result<AppendEntriesResponse<NID>, Infallible>>),
    ReceiveSnapshotChunk(ValueSender<Result<(), InstallSnapshotError>>),
    InstallSnapshot(ValueSender<Result<InstallSnapshotResponse<NID>, InstallSnapshotError>>),
    Initialize(ValueSender<Result<(), InitializeError<NID, N>>>),
}

impl<NID, N> Respond<NID, N>
where
    NID: NodeId,
    N: Node,
{
    pub(crate) fn new<T>(res: T, tx: oneshot::Sender<T>) -> Self
    where
        T: Debug + PartialEq + Eq,
        Self: From<ValueSender<T>>,
    {
        Respond::from(ValueSender::new(res, tx))
    }

    pub(crate) fn send(self) {
        match self {
            Respond::Vote(x) => x.send(),
            Respond::AppendEntries(x) => x.send(),
            Respond::ReceiveSnapshotChunk(x) => x.send(),
            Respond::InstallSnapshot(x) => x.send(),
            Respond::Initialize(x) => x.send(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ValueSender<T>
where T: Debug + PartialEq + Eq
{
    value: T,
    tx: oneshot::Sender<T>,
}

impl<T> PartialEq for ValueSender<T>
where T: Debug + PartialEq + Eq
{
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<T> Eq for ValueSender<T> where T: Debug + PartialEq + Eq {}

impl<T> ValueSender<T>
where T: Debug + PartialEq + Eq
{
    pub(crate) fn new(res: T, tx: oneshot::Sender<T>) -> Self {
        Self { value: res, tx }
    }

    pub(crate) fn send(self) {
        let _ = self.tx.send(self.value);
    }
}
