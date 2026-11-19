// SPDX-License-Identifier: MIT
//! One event-loop owner controls offers, session leases and installed native scenes.

use super::{
    backing::ImageDescriptor,
    pool::ImagePool,
    profile::Dialect,
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
impl From<io::Error> for AuthorityError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Clone, Copy)]
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
    rearm_deadline: Instant,
    stale_records: u8,
    last_end: EndReason,
    closed: bool,
    last_poll: Instant,
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
            rearm_deadline: now + OPERATION,
            stale_records: 0,
            last_end: EndReason::Release,
            closed: false,
            last_poll: now,
        })
    }

    pub fn epoch(&self) -> PtyEpoch {
        self.epoch
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
        self.stale_records = 0;
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
        self.session = None;
        self.offer = None;
        self.after_reply = None;
        self.last_end = reason;
        self.rearm_deadline = now + OPERATION;
        self.stale_records = 0;
        let result = self.endpoint.discard_reply();
        let next = self.epoch.get().checked_add(1).and_then(PtyEpoch::new);
        match (result, next) {
            (Ok(()), Some(next)) => self.epoch = next,
            (Err(error), _) => {
                self.close();
                return Err(error.into());
            }
            (_, None) => {
                self.close();
                return Err(AuthorityError::Counter);
            }
        }
        if legacy {
            self.close();
        }
        Ok(removed)
    }

    pub fn close(&mut self) {
        self.session = None;
        self.offer = None;
        self.after_reply = None;
        self.endpoint.close();
        self.closed = true;
    }

    /// One bounded frame copy and at most one framed control request per turn.
    pub fn poll(&mut self, now: Instant) -> Result<bool, AuthorityError> {
        self.last_poll = now;
        match self.poll_inner(now) {
            Ok(changed) => Ok(changed),
            Err(error) => {
                self.close();
                Err(error)
            }
        }
    }

    fn poll_inner(&mut self, now: Instant) -> Result<bool, AuthorityError> {
        if !self.available() {
            return Err(AuthorityError::Protocol("native endpoint unavailable"));
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
        let ready = if let Some(offer) = &mut self.offer {
            match &offer.delivery {
                OfferDelivery::Waiting(hello)
                    if now >= self.rearm_deadline.min(hello.started + OPERATION) =>
                {
                    return Err(AuthorityError::Protocol("native offer write deadline"));
                }
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
        if let Some(effect) = self.after_reply {
            if self.endpoint.flush(now)? == Flush::Pending {
                return Ok(changed);
            }
            self.after_reply = None;
            if let AfterReply::CloseLegacy = effect {
                self.close();
                return Ok(changed);
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
            }
        }
        if let Some(session) = &mut self.session {
            if let Phase::AwaitFrame(activation) = session.phase {
                if session
                    .frames
                    .prepare_next()
                    .map_err(AuthorityError::Scene)?
                {
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
                if session
                    .frames
                    .prepare_next()
                    .map_err(AuthorityError::Scene)?
                {
                    session
                        .frames
                        .install_next()
                        .map_err(AuthorityError::Scene)?;
                    changed = true;
                }
            }
        }
        let Receive::Request(request) = self.endpoint.poll(now)? else {
            return Ok(changed);
        };
        changed |= self.handle(request, now)?;
        Ok(changed)
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
            if self.stale_records == 4 || now >= self.rearm_deadline {
                return Err(AuthorityError::Protocol("native stale drain limit"));
            }
            self.stale_records += 1;
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
