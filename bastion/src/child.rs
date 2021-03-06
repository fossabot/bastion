//!
//! Child is a element of Children group executing user-defined computation
use crate::broadcast::Broadcast;
use crate::child_ref::ChildRef;
use crate::context::{BastionContext, BastionId, ContextState};
use crate::envelope::Envelope;
use crate::message::BastionMessage;
use crate::system::SYSTEM;
use bastion_executor::pool;
use futures::pending;
use futures::poll;
use futures::prelude::*;
use lightproc::prelude::*;
use lightproc::proc_state::EmptyProcState;
use qutex::Qutex;
use std::fmt::{self, Debug, Formatter};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

pub(crate) struct Init(pub(crate) Box<dyn Fn(BastionContext) -> Exec + Send + Sync>);
pub(crate) struct Exec(Pin<Box<dyn Future<Output = Result<(), ()>> + Send>>);

#[derive(Debug)]
pub(crate) struct Child {
    bcast: Broadcast,
    // The future that this child is executing.
    exec: Exec,
    // A lock behind which is the child's context state.
    // This is used to store the messages that were received
    // for the child's associated future to be able to
    // retrieve them.
    state: Qutex<ContextState>,
    // Messages that were received before the child was
    // started. Those will be "replayed" once a start message
    // is received.
    pre_start_msgs: Vec<Envelope>,
    // A shortcut for accessing to this actor by others.
    child_ref: ChildRef,
    started: bool,
}

impl Init {
    pub(crate) fn new<C, F>(init: C) -> Self
    where
        C: Fn(BastionContext) -> F + Send + Sync + 'static,
        F: Future<Output = Result<(), ()>> + Send + 'static,
    {
        let init = Box::new(move |ctx: BastionContext| {
            let fut = init(ctx);
            let exec = Box::pin(fut);

            Exec(exec)
        });

        Init(init)
    }
}

impl Child {
    pub(crate) fn new(
        exec: Exec,
        bcast: Broadcast,
        state: Qutex<ContextState>,
        child_ref: ChildRef,
    ) -> Self {
        debug!("Child({}): Initializing.", bcast.id());
        let pre_start_msgs = Vec::new();
        let started = false;

        Child {
            bcast,
            exec,
            state,
            pre_start_msgs,
            child_ref,
            started,
        }
    }

    fn stack(&self) -> ProcStack {
        trace!("Child({}): Creating ProcStack.", self.id());
        let id = self.bcast.id().clone();
        // FIXME: panics?
        let parent = self.bcast.parent().clone().into_children().unwrap();
        let path = self.bcast.path().clone();
        let sender = self.bcast.sender().clone();

        // FIXME: with_pid
        ProcStack::default().with_after_panic(move |_state: &mut EmptyProcState| {
            // FIXME: clones
            let id = id.clone();
            warn!("Child({}): Panicked.", id);

            let msg = BastionMessage::faulted(id);
            let env = Envelope::new(msg, path.clone(), sender.clone());
            // TODO: handle errors
            parent.send(env).ok();
        })
    }

    pub(crate) fn id(&self) -> &BastionId {
        self.bcast.id()
    }

    fn stopped(&mut self) {
        debug!("Child({}): Stopped.", self.id());
        self.remove_from_dispatchers();
        self.bcast.stopped();
    }

    fn faulted(&mut self) {
        debug!("Child({}): Faulted.", self.id());
        self.remove_from_dispatchers();
        self.bcast.faulted();
    }

    async fn handle(&mut self, env: Envelope) -> Result<(), ()> {
        match env {
            Envelope {
                msg: BastionMessage::Start,
                ..
            } => unreachable!(),
            Envelope {
                msg: BastionMessage::Stop,
                ..
            } => {
                self.stopped();

                return Err(());
            }
            Envelope {
                msg: BastionMessage::Kill,
                ..
            } => {
                self.stopped();

                return Err(());
            }
            // FIXME
            Envelope {
                msg: BastionMessage::Deploy(_),
                ..
            } => unimplemented!(),
            // FIXME
            Envelope {
                msg: BastionMessage::Prune { .. },
                ..
            } => unimplemented!(),
            // FIXME
            Envelope {
                msg: BastionMessage::SuperviseWith(_),
                ..
            } => unimplemented!(),
            Envelope {
                msg: BastionMessage::Message(msg),
                sign,
            } => {
                debug!("Child({}): Received a message: {:?}", self.id(), msg);
                let mut state = self.state.clone().lock_async().await.map_err(|_| ())?;
                state.push_msg(msg, sign);
            }
            // FIXME
            Envelope {
                msg: BastionMessage::Stopped { .. },
                ..
            } => unimplemented!(),
            // FIXME
            Envelope {
                msg: BastionMessage::Faulted { .. },
                ..
            } => unimplemented!(),
        }

        Ok(())
    }

    async fn run(mut self) {
        debug!("Child({}): Launched.", self.id());
        self.register_in_dispatchers();

        loop {
            match poll!(&mut self.bcast.next()) {
                // TODO: Err if started == true?
                Poll::Ready(Some(Envelope {
                    msg: BastionMessage::Start,
                    ..
                })) => {
                    trace!(
                        "Child({}): Received a new message (started=false): {:?}",
                        self.id(),
                        BastionMessage::Start
                    );
                    debug!("Child({}): Starting.", self.id());
                    self.started = true;

                    let msgs = self.pre_start_msgs.drain(..).collect::<Vec<_>>();
                    self.pre_start_msgs.shrink_to_fit();

                    debug!(
                        "Child({}): Replaying messages received before starting.",
                        self.id()
                    );
                    for msg in msgs {
                        trace!("Child({}): Replaying message: {:?}", self.id(), msg);
                        if self.handle(msg).await.is_err() {
                            return;
                        }
                    }

                    continue;
                }
                Poll::Ready(Some(msg)) if !self.started => {
                    trace!(
                        "Child({}): Received a new message (started=false): {:?}",
                        self.id(),
                        msg
                    );
                    self.pre_start_msgs.push(msg);

                    continue;
                }
                Poll::Ready(Some(msg)) => {
                    trace!(
                        "Child({}): Received a new message (started=true): {:?}",
                        self.id(),
                        msg
                    );
                    if self.handle(msg).await.is_err() {
                        return;
                    }

                    continue;
                }
                // NOTE: because `Broadcast` always holds both a `Sender` and
                //      `Receiver` of the same channel, this would only be
                //      possible if the channel was closed, which never happens.
                Poll::Ready(None) => unreachable!(),
                Poll::Pending => (),
            }

            if !self.started {
                pending!();

                continue;
            }

            match poll!(&mut self.exec) {
                Poll::Ready(Ok(())) => {
                    debug!(
                        "Child({}): The future finished executing successfully.",
                        self.id()
                    );
                    return self.stopped();
                }
                Poll::Ready(Err(())) => {
                    warn!("Child({}): The future returned an error.", self.id());
                    return self.faulted();
                }
                Poll::Pending => (),
            }

            pending!();
        }
    }

    pub(crate) fn launch(self) -> RecoverableHandle<()> {
        let stack = self.stack();
        pool::spawn(self.run(), stack)
    }

    /// Adds the actor into each registry declared in the parent node.
    fn register_in_dispatchers(&self) {
        if let Some(parent) = self.bcast.parent().clone().into_children() {
            let child_ref = self.child_ref.clone();
            let used_dispatchers = parent.dispatchers();

            let global_dispatcher = SYSTEM.dispatcher();
            // FIXME: Pass the module name explicitly?
            let module_name = module_path!().to_string();
            global_dispatcher.register(used_dispatchers, &child_ref, module_name);
        }
    }

    /// Cleanup the actor's record from each declared dispatcher.
    fn remove_from_dispatchers(&self) {
        if let Some(parent) = self.bcast.parent().clone().into_children() {
            let child_ref = self.child_ref.clone();
            let used_dispatchers = parent.dispatchers();

            let global_dispatcher = SYSTEM.dispatcher();
            global_dispatcher.remove(used_dispatchers, &child_ref);
        }
    }
}

impl Future for Exec {
    type Output = Result<(), ()>;

    fn poll(self: Pin<&mut Self>, ctx: &mut Context) -> Poll<Self::Output> {
        Pin::new(&mut self.get_mut().0).poll(ctx)
    }
}

impl Default for Init {
    fn default() -> Self {
        Init::new(|_| async { Ok(()) })
    }
}

impl Debug for Init {
    fn fmt(&self, fmt: &mut Formatter) -> fmt::Result {
        fmt.debug_struct("Init").finish()
    }
}

impl Debug for Exec {
    fn fmt(&self, fmt: &mut Formatter) -> fmt::Result {
        fmt.debug_struct("Exec").finish()
    }
}
