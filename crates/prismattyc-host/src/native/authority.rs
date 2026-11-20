// SPDX-License-Identifier: MIT
//! One owner controls offers, session leases and installed native scenes.

use super::{
    backing::ImageDescriptor,
    pool::ImagePool,
    profile::Dialect,
    readiness::{Readiness, Wake},
    registry::RegistryError,
    ring::RingContext,
    scene::{NativeScene, SceneError, SessionFrames},
    transport::{ControlEndpoint, Flush, OwnedRequest, Receive, AVAILABILITY},
    wire12::{
        BackingKind, EndReason, FrameSequence, Metrics, MetricsEpoch, PtyEpoch, Reply,
        ReplyIdentity, Request, RingExtent, SessionId,
    },
};
use std::{
    io,
    num::NonZeroU32,
    os::unix::net::UnixStream,
    sync::Arc,
    time::{Duration, Instant},
};

const OPERATION: Duration = Duration::from_millis(200);
const FRAME_POLL: Duration = Duration::from_millis(20);
const LEASE: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub enum AuthorityError {
    Io(io::Error),
    Geometry,
    Counter,
    Protocol(&'static str),
    Scene(SceneError),
    Resource(RegistryError),
}
impl std::fmt::Display for AuthorityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "native transport: {error}"),
            Self::Geometry => formatter.write_str("native geometry is unavailable"),
            Self::Counter => formatter.write_str("native sequence is exhausted"),
            Self::Protocol(reason) => write!(formatter, "native protocol: {reason}"),
            Self::Scene(error) => write!(formatter, "native scene: {error:?}"),
            Self::Resource(error) => write!(formatter, "native image: {error:?}"),
        }
    }
}
impl std::error::Error for AuthorityError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for AuthorityError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Geometry {
    columns: u32,
    rows: u32,
    cell_width: u32,
    cell_height: u32,
}
impl Geometry {
    pub(super) fn matches_grid(self, columns: u32, rows: u32) -> bool {
        self.columns == columns && self.rows == rows
    }
    pub fn new(columns: u32, rows: u32, cell_width: u32, cell_height: u32) -> Option<Self> {
        Metrics::new(
            MetricsEpoch::new(1)?,
            columns,
            rows,
            cell_width,
            cell_height,
        )?;
        Some(Self {
            columns,
            rows,
            cell_width,
            cell_height,
        })
    }
    fn metrics(self, epoch: MetricsEpoch) -> Metrics {
        Metrics::new(
            epoch,
            self.columns,
            self.rows,
            self.cell_width,
            self.cell_height,
        )
        .unwrap()
    }
}

#[derive(Clone, Debug)]
pub struct OfferReceipt(Arc<()>);
impl PartialEq for OfferReceipt {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for OfferReceipt {}

pub struct PreparedOffer {
    pub capability_id: NonZeroU32,
    pub epoch: PtyEpoch,
    pub value: [u8; 16],
    pub receipt: OfferReceipt,
}
struct Offer {
    capability_id: NonZeroU32,
    value: [u8; 16],
    receipt: OfferReceipt,
    delivery: OfferDelivery,
}

enum OfferDelivery {
    Queued,
    Written,
    Waiting(OwnedRequest),
    Ready(OwnedRequest),
}

#[derive(Clone, Copy)]
struct Activation {
    identity: ReplyIdentity,
    started: Instant,
    validated_at: Instant,
    deadline: Instant,
}
enum Phase {
    Setup {
        deadline: Instant,
    },
    AwaitFrame(Activation),
    AwaitActiveReply {
        deadline: Instant,
        lease_until: Instant,
    },
    Active {
        lease_until: Instant,
    },
}
struct Session {
    id: SessionId,
    dialect: Dialect,
    next_request: u64,
    frames: SessionFrames,
    phase: Phase,
}
#[derive(Clone, Copy)]
enum AfterReply {
    Ordinary,
    Activate,
    CloseLegacy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Installation {
    session: SessionId,
    epoch: PtyEpoch,
    metrics: MetricsEpoch,
    frame: FrameSequence,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum FrameWork {
    Keep,
    Prepare,
    InstallPending,
    #[cfg(test)]
    Install,
}
impl FrameWork {
    fn copies(self) -> bool {
        match self {
            Self::Keep | Self::InstallPending => false,
            Self::Prepare => true,
            #[cfg(test)]
            Self::Install => true,
        }
    }
    fn installs(self) -> bool {
        match self {
            Self::Keep | Self::Prepare => false,
            Self::InstallPending => true,
            #[cfg(test)]
            Self::Install => true,
        }
    }
}

struct StaleDrain {
    deadline: Instant,
    records: u8,
}

pub struct DesktopNative {
    endpoint: ControlEndpoint<'static>,
    geometry: Geometry,
    epoch: PtyEpoch,
    next_session: Option<SessionId>,
    next_metrics: Option<MetricsEpoch>,
    pool: Arc<ImagePool>,
    offer: Option<Offer>,
    session: Option<Session>,
    after_reply: Option<AfterReply>,
    acknowledged_installation: Option<Installation>,
    rearm_deadline: Instant,
    stale_drain: Option<StaleDrain>,
    last_end: EndReason,
    closed: bool,
    last_poll: Instant,
    readiness: Option<Readiness>,
}

impl DesktopNative {
    /// The caller must supply the retained endpoint of its own direct PTY launch.
    pub fn new(
        socket: UnixStream,
        geometry: Geometry,
        now: Instant,
    ) -> Result<Self, AuthorityError> {
        Ok(Self {
            endpoint: ControlEndpoint::new(socket)?,
            geometry,
            epoch: PtyEpoch::new(1).unwrap(),
            next_session: SessionId::new(1),
            next_metrics: MetricsEpoch::new(1),
            pool: ImagePool::global(),
            offer: None,
            session: None,
            after_reply: None,
            acknowledged_installation: None,
            rearm_deadline: now + OPERATION,
            stale_drain: None,
            last_end: EndReason::Release,
            closed: false,
            last_poll: now,
            readiness: None,
        })
    }

    pub fn epoch(&self) -> PtyEpoch {
        self.epoch
    }
    pub fn geometry(&self) -> Geometry {
        self.geometry
    }
    pub fn scene(&self) -> Option<&Arc<NativeScene>> {
        if !self.available() {
            return None;
        }
        self.session
            .as_ref()
            .and_then(|session| session.frames.installed())
    }
    pub fn available(&self) -> bool {
        !self.closed && AVAILABILITY.available()
    }

    pub fn start_watcher(&mut self, wake: Wake) -> io::Result<()> {
        if self.readiness.is_some() {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        let watcher = self.endpoint.watch(wake)?;
        watcher.set_armed(self.can_receive())?;
        self.readiness = Some(watcher);
        Ok(())
    }

    fn rearm_readiness(&mut self) -> Result<(), AuthorityError> {
        if let Some(watcher) = &self.readiness {
            if let Err(error) = watcher.set_armed(self.can_receive()) {
                self.close();
                return Err(error.into());
            }
        }
        Ok(())
    }

    fn can_receive(&self) -> bool {
        self.available()
            && self.after_reply.is_none()
            && !self.offer.as_ref().is_some_and(|offer| {
                matches!(
                    offer.delivery,
                    OfferDelivery::Waiting(_) | OfferDelivery::Ready(_)
                )
            })
            && !self
                .session
                .as_ref()
                .is_some_and(|session| matches!(session.phase, Phase::AwaitFrame(_)))
    }

    /// The application combines this deadline with control-socket readiness.
    /// Unoffered, inactive endpoints schedule no periodic work.
    pub fn next_wakeup(&self) -> Option<Instant> {
        if !self.available() {
            return None;
        }
        let mut wake = self.endpoint.deadline();
        let mut include = |at: Instant| wake = Some(wake.map_or(at, |old| old.min(at)));
        if self.after_reply.is_some() {
            include(self.last_poll + FRAME_POLL);
        }
        if let Some(offer) = &self.offer {
            include(match &offer.delivery {
                OfferDelivery::Waiting(hello) => self.rearm_deadline.min(hello.started + OPERATION),
                OfferDelivery::Ready(_) => self.last_poll,
                _ => self.rearm_deadline,
            });
        }
        if let Some(session) = &self.session {
            match session.phase {
                Phase::Setup { deadline } | Phase::AwaitActiveReply { deadline, .. } => {
                    include(deadline)
                }
                Phase::AwaitFrame(activation) => {
                    include(activation.deadline);
                    include(self.last_poll + FRAME_POLL);
                }
                Phase::Active { lease_until } => {
                    include(lease_until);
                    include(self.last_poll + FRAME_POLL);
                }
            }
        }
        wake
    }

    pub fn prepare_offer(
        &mut self,
        capability_id: NonZeroU32,
        now: Instant,
    ) -> Result<PreparedOffer, AuthorityError> {
        if !self.available()
            || self.session.is_some()
            || matches!(self.after_reply, Some(AfterReply::CloseLegacy))
        {
            return Err(AuthorityError::Protocol("native offer state"));
        }
        let mut value = [0; 16];
        getrandom::fill(&mut value)
            .map_err(|_| AuthorityError::Protocol("native offer entropy"))?;
        let receipt = OfferReceipt(Arc::new(()));
        self.offer = Some(Offer {
            capability_id,
            value,
            receipt: receipt.clone(),
            delivery: OfferDelivery::Queued,
        });
        self.rearm_deadline = now + OPERATION;
        self.stale_drain = None;
        self.rearm_readiness()?;
        Ok(PreparedOffer {
            capability_id,
            epoch: self.epoch,
            value,
            receipt,
        })
    }

    /// Called only by the PTY writer's successful full-write receipt path.
    pub fn authorize_written_offer(&mut self, receipt: OfferReceipt) {
        if let Some(offer) = &mut self.offer {
            if offer.receipt == receipt {
                offer.delivery = match std::mem::replace(&mut offer.delivery, OfferDelivery::Queued)
                {
                    OfferDelivery::Queued => OfferDelivery::Written,
                    OfferDelivery::Waiting(hello) => OfferDelivery::Ready(hello),
                    other => other,
                };
            }
        }
    }

    /// Call before parsing any PTY bytes, including a control-only capability query.
    pub fn pty_bytes(&mut self, now: Instant) -> Result<bool, AuthorityError> {
        self.revoke(EndReason::PtyOutput, now)
    }
    /// Revocation precedes all changes to PTY and physical metrics.
    pub fn resize(&mut self, geometry: Geometry, now: Instant) -> Result<bool, AuthorityError> {
        let removed = self.revoke(EndReason::Resize, now)?;
        self.geometry = geometry;
        Ok(removed)
    }

    fn revoke(&mut self, reason: EndReason, now: Instant) -> Result<bool, AuthorityError> {
        let removed = self.scene().is_some();
        let legacy = self
            .session
            .as_ref()
            .is_some_and(|session| session.dialect == Dialect::Grid0)
            || matches!(self.after_reply, Some(AfterReply::CloseLegacy));
        let activation = self
            .session
            .as_ref()
            .and_then(|session| match session.phase {
                Phase::AwaitFrame(activation) => Some(activation),
                _ => None,
            });
        self.session = None;
        self.offer = None;
        self.after_reply = None;
        self.acknowledged_installation = None;
        self.last_end = reason;
        self.rearm_deadline = now + OPERATION;
        self.stale_drain = None;
        let Some(next) = self.epoch.get().checked_add(1).and_then(PtyEpoch::new) else {
            self.close();
            return Err(AuthorityError::Counter);
        };
        self.epoch = next;
        let pending = self.endpoint.revoke_reply(next, reason);
        if pending {
            self.after_reply = Some(if legacy {
                AfterReply::CloseLegacy
            } else {
                AfterReply::Ordinary
            });
        } else if let Some(activation) = activation.filter(|_| !legacy) {
            self.queue(
                Dialect::Images2,
                activation.identity,
                Reply::SessionEnded {
                    current_epoch: next,
                    reason,
                },
                activation.started,
            )?;
        }
        if legacy && !pending {
            self.close();
        }
        self.rearm_readiness()?;
        Ok(removed)
    }

    pub fn close(&mut self) {
        self.readiness = None;
        self.session = None;
        self.offer = None;
        self.after_reply = None;
        self.acknowledged_installation = None;
        self.endpoint.close();
        self.closed = true;
    }

    /// Synchronous authority fixture; production advances through NativeService.
    #[cfg(test)]
    pub fn poll(&mut self, now: Instant) -> Result<bool, AuthorityError> {
        self.poll_mode(now, FrameWork::Install, false)
    }

    pub(super) fn poll_for_service(
        &mut self,
        now: Instant,
        work: FrameWork,
    ) -> Result<bool, AuthorityError> {
        self.poll_mode(now, work, true)
    }

    fn poll_mode(
        &mut self,
        now: Instant,
        work: FrameWork,
        defer_activation: bool,
    ) -> Result<bool, AuthorityError> {
        self.last_poll = now;
        let result = self
            .poll_turn(now, work, defer_activation)
            .and_then(|changed| {
                self.rearm_readiness()?;
                Ok(changed)
            });
        match result {
            Ok(changed) => Ok(changed),
            Err(error) => {
                self.close();
                Err(error)
            }
        }
    }

    fn expire_inner(&mut self, now: Instant) -> Result<bool, AuthorityError> {
        if !self.available() {
            return Err(AuthorityError::Protocol("native endpoint unavailable"));
        }
        if self.readiness.as_ref().is_some_and(Readiness::failed) {
            return Err(AuthorityError::Protocol("native readiness watcher failed"));
        }
        let mut changed = false;
        if let Some(session) = &self.session {
            match session.phase {
                Phase::Active { lease_until } if now >= lease_until => {
                    changed |= self.revoke(EndReason::LeaseExpired, now)?;
                }
                Phase::Setup { deadline } | Phase::AwaitActiveReply { deadline, .. }
                    if now >= deadline =>
                {
                    return Err(AuthorityError::Protocol("native setup deadline"))
                }
                Phase::AwaitFrame(activation) if now >= activation.deadline => {
                    return Err(AuthorityError::Protocol("native activation deadline"));
                }
                _ => {}
            }
        }
        if self.closed {
            return Ok(changed);
        }
        if self.offer.as_ref().is_some_and(|offer| {
            matches!(
                offer.delivery,
                OfferDelivery::Queued | OfferDelivery::Written
            ) && now >= self.rearm_deadline
        }) {
            self.offer = None;
        }
        if self
            .offer
            .as_ref()
            .is_some_and(|offer| match &offer.delivery {
                OfferDelivery::Waiting(hello) | OfferDelivery::Ready(hello) => {
                    now >= self.rearm_deadline.min(hello.started + OPERATION)
                }
                _ => false,
            })
        {
            return Err(AuthorityError::Protocol("native offer write deadline"));
        }
        if self
            .endpoint
            .deadline()
            .is_some_and(|deadline| now >= deadline)
        {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "native control deadline").into());
        }
        Ok(changed)
    }

    /// Deadline-only fixture; the production worker enforces these in every turn.
    #[cfg(test)]
    pub fn expire(&mut self, now: Instant) -> Result<bool, AuthorityError> {
        let result = self.expire_inner(now).and_then(|changed| {
            self.rearm_readiness()?;
            Ok(changed)
        });
        match result {
            Ok(changed) => Ok(changed),
            Err(error) => {
                self.close();
                Err(error)
            }
        }
    }

    fn poll_turn(
        &mut self,
        now: Instant,
        work: FrameWork,
        defer_activation: bool,
    ) -> Result<bool, AuthorityError> {
        let mut changed = self.expire_inner(now)?;
        if self.closed {
            return Ok(changed);
        }
        if self.activation_needs_ack(defer_activation) {
            return Ok(changed);
        }
        if self.flush_reply(now)? == Flush::Pending || self.closed {
            return Ok(changed);
        }
        changed |= self.poll_inner(now, work)?;
        // A ready reply must reach the peer before rasterization or presentation.
        // A blocked write retains its original absolute deadline for the next turn.
        if !self.activation_needs_ack(defer_activation) {
            self.flush_reply(now)?;
        }
        Ok(changed)
    }

    fn poll_inner(&mut self, now: Instant, work: FrameWork) -> Result<bool, AuthorityError> {
        let mut changed = false;
        let ready = if let Some(offer) = &mut self.offer {
            match &offer.delivery {
                OfferDelivery::Waiting(_) => return Ok(changed),
                OfferDelivery::Ready(_) => {
                    let OfferDelivery::Ready(hello) =
                        std::mem::replace(&mut offer.delivery, OfferDelivery::Written)
                    else {
                        unreachable!()
                    };
                    Some(hello)
                }
                _ => None,
            }
        } else {
            None
        };
        if let Some(hello) = ready {
            changed |= self.handle(hello, now)?;
            return Ok(changed);
        }
        if let Some(session) = &mut self.session {
            if let Phase::AwaitFrame(activation) = session.phase {
                if work.copies() && !session.frames.has_pending() {
                    session
                        .frames
                        .prepare_next()
                        .map_err(AuthorityError::Scene)?;
                }
                if work.installs() && session.frames.has_pending() {
                    let installed = session
                        .frames
                        .install_next()
                        .map_err(AuthorityError::Scene)?;
                    if installed != FrameSequence::new(1) {
                        return Err(AuthorityError::Protocol("initial native installation"));
                    }
                    changed = true;
                    session.phase = Phase::AwaitActiveReply {
                        deadline: activation.deadline,
                        lease_until: activation.validated_at + LEASE,
                    };
                    let reply = Reply::Active {
                        first_frame: FrameSequence::new(1).unwrap(),
                        attributes: false,
                    };
                    match session.dialect {
                        Dialect::Grid0 => self.endpoint.queue_legacy_reply(
                            activation.identity,
                            reply,
                            None,
                            activation.started,
                        )?,
                        Dialect::Images2 => self.endpoint.queue_reply(
                            activation.identity,
                            reply,
                            None,
                            activation.started,
                        )?,
                    }
                    self.after_reply = Some(AfterReply::Activate);
                }
                return Ok(changed);
            }
            if matches!(session.phase, Phase::Active { .. }) {
                session.frames.pong(now).map_err(AuthorityError::Resource)?;
                if work.copies() {
                    session
                        .frames
                        .prepare_next()
                        .map_err(AuthorityError::Scene)?;
                }
                if work.installs() && session.frames.has_pending() {
                    session
                        .frames
                        .install_next()
                        .map_err(AuthorityError::Scene)?;
                    changed = true;
                }
            }
        }
        if work == FrameWork::Prepare {
            return Ok(changed);
        }
        let Receive::Request(request) = self.endpoint.poll(now)? else {
            return Ok(changed);
        };
        changed |= self.handle(request, now)?;
        Ok(changed)
    }

    pub(super) fn installation(&self) -> Option<Installation> {
        let session = self.session.as_ref()?;
        let context = session.frames.installed()?.frame().context;
        Some(Installation {
            session: session.id,
            epoch: context.epoch,
            metrics: context.metrics,
            frame: context.sequence,
        })
    }

    pub(super) fn pending_installation(&self) -> Option<Installation> {
        self.activation_needs_ack(true)
            .then(|| self.installation())
            .flatten()
    }

    pub(super) fn frame_backlog_ready(&self) -> bool {
        self.after_reply.is_none()
            && self.session.as_ref().is_some_and(|session| {
                matches!(session.phase, Phase::Active { .. })
                    && session.frames.has_unconsumed_frame()
            })
    }

    fn activation_needs_ack(&self, defer: bool) -> bool {
        defer
            && matches!(self.after_reply, Some(AfterReply::Activate))
            && self.acknowledged_installation != self.installation()
    }

    pub(super) fn acknowledge_installation(
        &mut self,
        token: Installation,
        now: Instant,
    ) -> Result<bool, AuthorityError> {
        self.expire_inner(now)?;
        if self.installation() != Some(token) || token.epoch != self.epoch {
            return Ok(false);
        }
        self.acknowledged_installation = Some(token);
        self.flush_reply(now)?;
        self.rearm_readiness()?;
        Ok(true)
    }

    pub(super) fn has_pending_frame(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| session.frames.has_pending())
    }

    #[cfg(test)]
    pub(super) fn pending_sequence(&self) -> Option<FrameSequence> {
        self.session.as_ref()?.frames.pending_sequence()
    }

    #[cfg(test)]
    pub(super) fn awaiting_initial_frame(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| matches!(session.phase, Phase::AwaitFrame(_)))
    }

    fn flush_reply(&mut self, now: Instant) -> Result<Flush, AuthorityError> {
        if let Some(effect) = self.after_reply {
            if self.endpoint.flush(now)? == Flush::Pending {
                return Ok(Flush::Pending);
            }
            self.after_reply = None;
            if let AfterReply::CloseLegacy = effect {
                self.close();
                return Ok(Flush::Complete);
            }
            if let AfterReply::Activate = effect {
                let session = self
                    .session
                    .as_mut()
                    .ok_or(AuthorityError::Protocol("missing active reply owner"))?;
                let Phase::AwaitActiveReply { lease_until, .. } = session.phase else {
                    return Err(AuthorityError::Protocol("active reply state"));
                };
                session.phase = Phase::Active { lease_until };
                self.stale_drain = None;
            }
        }
        Ok(Flush::Complete)
    }

    fn queue(
        &mut self,
        dialect: Dialect,
        identity: ReplyIdentity,
        reply: Reply,
        started: Instant,
    ) -> Result<(), AuthorityError> {
        match dialect {
            Dialect::Grid0 => self
                .endpoint
                .queue_legacy_reply(identity, reply, None, started)?,
            Dialect::Images2 => self.endpoint.queue_reply(identity, reply, None, started)?,
        }
        self.after_reply = Some(AfterReply::Ordinary);
        Ok(())
    }

    fn handle(&mut self, request: OwnedRequest, now: Instant) -> Result<bool, AuthorityError> {
        let header = request.record.header;
        if header.epoch < self.epoch && header.session.is_some() {
            let deadline = request.started + OPERATION;
            let deadline = if self.offer.is_some() {
                deadline.min(self.rearm_deadline)
            } else {
                deadline
            };
            let drain = self.stale_drain.get_or_insert(StaleDrain {
                deadline,
                records: 0,
            });
            if drain.records == 4 || now >= drain.deadline {
                return Err(AuthorityError::Protocol("native stale drain limit"));
            }
            drain.records += 1;
            self.queue(
                Dialect::Images2,
                ReplyIdentity {
                    session: header.session.unwrap(),
                    sequence: header.sequence,
                    epoch: header.epoch,
                },
                Reply::SessionEnded {
                    current_epoch: self.epoch,
                    reason: self.last_end,
                },
                request.started,
            )?;
            return Ok(false);
        }
        if header.epoch != self.epoch {
            return Err(AuthorityError::Protocol("native PTY epoch"));
        }
        if let Request::Hello {
            capability_id,
            offer,
            attributes,
        } = request.record.request
        {
            let dialect = request
                .hello_dialect
                .ok_or(AuthorityError::Protocol("native HELLO dialect"))?;
            if !dialect.supported() {
                return Err(AuthorityError::Protocol("native unsupported dialect"));
            }
            if self.session.is_some() || attributes || now >= self.rearm_deadline {
                return Err(AuthorityError::Protocol("native HELLO state"));
            }
            let expected = self
                .offer
                .as_mut()
                .ok_or(AuthorityError::Protocol("native missing offer"))?;
            if expected.capability_id != capability_id || expected.value != offer {
                return Err(AuthorityError::Protocol("native unwritten or stale offer"));
            }
            if !matches!(expected.delivery, OfferDelivery::Written) {
                if !matches!(expected.delivery, OfferDelivery::Queued) {
                    return Err(AuthorityError::Protocol("native outstanding HELLO"));
                }
                expected.delivery = OfferDelivery::Waiting(request);
                return Ok(false);
            }
            self.offer = None;
            let id = self.next_session.ok_or(AuthorityError::Counter)?;
            let metrics_epoch = self.next_metrics.ok_or(AuthorityError::Counter)?;
            let context = RingContext {
                session: id,
                epoch: self.epoch,
                metrics: metrics_epoch,
                columns: self.geometry.columns,
                rows: self.geometry.rows,
            };
            let frames = match dialect {
                Dialect::Grid0 => SessionFrames::new_legacy(context, Arc::clone(&self.pool))?,
                Dialect::Images2 => SessionFrames::new(context, Arc::clone(&self.pool))?,
            };
            let kind = if cfg!(target_os = "macos") {
                BackingKind::DarwinFixedPosix
            } else {
                BackingKind::LinuxSealed
            };
            let reply = Reply::Setup {
                ring: RingExtent::new(kind, frames.physical_bytes())
                    .ok_or(AuthorityError::Geometry)?,
                metrics: self.geometry.metrics(metrics_epoch),
            };
            let identity = ReplyIdentity {
                session: id,
                sequence: header.sequence,
                epoch: self.epoch,
            };
            let descriptor = Some(frames.descriptor().try_clone_to_owned()?);
            match dialect {
                Dialect::Grid0 => self.endpoint.queue_legacy_reply(
                    identity,
                    reply,
                    descriptor,
                    request.started,
                )?,
                Dialect::Images2 => {
                    self.endpoint
                        .queue_reply(identity, reply, descriptor, request.started)?
                }
            }
            self.after_reply = Some(AfterReply::Ordinary);
            self.session = Some(Session {
                id,
                dialect,
                next_request: 2,
                frames,
                phase: Phase::Setup {
                    deadline: request.started + OPERATION,
                },
            });
            self.next_session = id.get().checked_add(1).and_then(SessionId::new);
            self.next_metrics = metrics_epoch
                .get()
                .checked_add(1)
                .and_then(MetricsEpoch::new);
            return Ok(false);
        }
        let mut session = self
            .session
            .take()
            .ok_or(AuthorityError::Protocol("native request without session"))?;
        if header.session != Some(session.id) || header.sequence.get() != session.next_request {
            return Err(AuthorityError::Protocol("native session or sequence"));
        }
        session.next_request = session
            .next_request
            .checked_add(1)
            .ok_or(AuthorityError::Counter)?;
        let identity = ReplyIdentity {
            session: session.id,
            sequence: header.sequence,
            epoch: self.epoch,
        };
        let active = matches!(session.phase, Phase::Active { .. });
        match request.record.request {
            Request::Activate(first)
                if matches!(session.phase, Phase::Setup { .. }) && first.get() == 1 =>
            {
                let Phase::Setup { deadline } = session.phase else {
                    unreachable!()
                };
                session.phase = Phase::AwaitFrame(Activation {
                    identity,
                    started: request.started,
                    validated_at: now,
                    deadline: deadline.min(request.started + OPERATION),
                });
                self.session = Some(session);
                Ok(false)
            }
            Request::Release if active || matches!(session.phase, Phase::Setup { .. }) => {
                let removed = session.frames.installed().is_some();
                let dialect = session.dialect;
                drop(session);
                self.revoke(EndReason::Release, now)?;
                self.queue(dialect, identity, Reply::Released, request.started)?;
                if dialect == Dialect::Grid0 {
                    self.after_reply = Some(AfterReply::CloseLegacy);
                }
                Ok(removed)
            }
            operation if active => {
                let dialect = session.dialect;
                if dialect == Dialect::Grid0 && !matches!(operation, Request::Ping) {
                    return Err(AuthorityError::Protocol("legacy native active opcode"));
                }
                let reply = match operation {
                    Request::Ping => {
                        Reply::Pong(session.frames.pong(now).map_err(AuthorityError::Resource)?)
                    }
                    Request::PutImage(metadata) => {
                        let source = ImageDescriptor::validate(
                            request
                                .descriptor
                                .ok_or(AuthorityError::Protocol("native image FD"))?,
                            metadata,
                        )?;
                        session
                            .frames
                            .put(source, now)
                            .map_err(AuthorityError::Resource)?
                    }
                    Request::RetireImage {
                        handle,
                        omitting_frame,
                    } => Reply::Retired {
                        handle,
                        status: session
                            .frames
                            .retire(handle, omitting_frame)
                            .map_err(AuthorityError::Resource)?,
                    },
                    _ => return Err(AuthorityError::Protocol("native active opcode")),
                };
                session.phase = Phase::Active {
                    lease_until: now + LEASE,
                };
                self.session = Some(session);
                self.queue(dialect, identity, reply, request.started)?;
                Ok(false)
            }
            _ => Err(AuthorityError::Protocol("native request state")),
        }
    }
}

#[cfg(test)]
#[path = "authority_tests.rs"]
mod tests;
