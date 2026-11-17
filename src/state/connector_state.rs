use crate::state::{ConnectorEvent, ConnectorStatus};

/// A connector's internal state machine, protocol-version-independent - version adapters project
/// this down to each OCPP version's own status enum (see
/// [`ConnectorState::availability_status`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorState {
    /// No cable connected, not reserved, no fault.
    Available,
    /// A cable is physically plugged in; the connector is being locked.
    Connected,
    /// The cable is locked in place, awaiting an identifier to be presented or a remote command.
    Locked,
    /// An identifier was presented and the CSMS's authorization decision is pending.
    Authorizing,
    /// Authorization succeeded (or a remote start was requested); the contactor is closing.
    Starting,
    /// The contactor is closed and energy is flowing.
    Charging,
    /// The contactor is closed but the **EV** has stopped drawing energy - a full battery, or a
    /// vehicle-side pause. The transaction is still running and the cable still locked; only the
    /// energy flow has paused, and the EV can resume it on its own.
    ///
    /// Reported by the hardware binding, which is the only thing that can tell *which side*
    /// stopped drawing (`docs/PRODUCTION-ROADMAP.md` B1.5). 1.6J has a wire status for this;
    /// 2.x moved the distinction onto the transaction's `chargingState`, so both versions can
    /// express it - see [`ConnectorStatus`] for why this crate's connector *status* cannot.
    SuspendedEv,
    /// The contactor is closed but the **EVSE** has stopped supplying energy - a smart-charging
    /// limit of 0 A, load management, or a local supply constraint. The transaction is still
    /// running; the charge point resumes it when whatever imposed the pause lifts.
    SuspendedEvse,
    /// Charging has stopped and the contactor is opening; the cable is still locked.
    Stopping,
    /// Charging has stopped after the cable left the **EV** on a station whose
    /// `OCPPCommCtrlr.UnlockOnEVSideDisconnect` is `false`, and the contactor is opening.
    ///
    /// Identical to [`Self::Stopping`] in every respect except how it ends: E09.FR.03 says a
    /// station configured this way keeps hold of its own end of the cable, so this settles to
    /// [`Self::Locked`] rather than unlocking through [`Self::Finishing`]. It is a separate state
    /// because this crate's connector transition is a pure function of `(state, event, policy)` -
    /// by the time the contactor confirms open, the event that caused the stop is long gone, so
    /// the decision has to be carried in the state itself.
    StoppingLocked,
    /// The transaction ended and the contactor is open; the cable may still be plugged in.
    /// This is the connector's post-session rest state while the bay stays occupied - it
    /// exits only when the cable leaves (`CableDisconnected` → `Available`), which is what
    /// 1.6J's `Finishing` wire status means and what an operator billing for bay occupancy
    /// needs to see. A new session can begin right from here: the cable is already in, so
    /// a remote start or a re-presented identifier behaves exactly as it does on `Locked`.
    Finishing,
    /// Made unavailable (OCPP `ChangeAvailability`); not usable until made available again.
    Unavailable,
    /// A hardware fault was detected; the contactor is being (or has been) forced open.
    Faulted,
    /// A hardware fault was detected and the contactor has confirmed open - safe to unlock once
    /// the fault clears.
    FaultedSafe,
    /// The connector is unlocking, either after a normal session end or fault recovery.
    Unlocking,
    /// The CSMS has reserved this connector (OCPP `ReserveNow`) for a specific id token; no
    /// cable is connected yet. See `docs/ROADMAP.md` §8.
    Reserved,
}

pub(crate) enum ConnectorCommand {
    Lock,
    Unlock,
    CloseContactor,
    OpenContactor,
}

pub(crate) struct ConnectorTransition {
    pub changed: bool,
    pub command: Option<ConnectorCommand>,
}

/// Where in a session OCPP says the transaction begins - `TxCtrlr.TxStartPoint`
/// (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV2.2).
///
/// OCPP models this as a *set*: the transaction starts once every configured condition holds. The
/// three this charge point can observe are strictly ordered along a session, so "all conditions
/// met" is simply the last of them - which is why this is an ordered enum and
/// [`Self::from_member_list`] takes the maximum rather than tracking a set of flags.
///
/// The other three values OCPP defines are not here, and their absence is enforced rather than
/// implied: `ParkingBayOccupancy` needs a bay sensor this crate has no binding for, `DataSigned`
/// needs signed meter values it does not produce, and `EnergyTransfer` needs a "current is
/// actually flowing" signal distinct from the contactor being closed. The variable's declared
/// `values_list` is narrowed to the three below, so a `SetVariables` naming one of the others is
/// `Rejected` by CV3's validation with a reason - rather than accepted and quietly treated as
/// something else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub(crate) enum TxStartPoint {
    /// The cable is connected and locked - the transaction covers the whole time the bay is
    /// occupied, including any authorization attempt that fails.
    EVConnected,
    /// The presented identifier was authorized. OCPP's default, and this crate's.
    #[default]
    Authorized,
    /// The contactor closed - the transaction covers only the time energy could actually flow.
    PowerPathClosed,
}

impl TxStartPoint {
    /// Parses `TxCtrlr.TxStartPoint`'s `MemberList` value, taking the latest point named.
    ///
    /// An empty or unrecognised value falls back to the default rather than erroring: the value
    /// reaching here has already passed CV3's validation against the declared `values_list`, so an
    /// unparseable one means the device model was written by something other than `SetVariables`
    /// (a hardware binding, a persisted restore), and a charge point is better off starting
    /// transactions the conventional way than not at all.
    pub fn from_member_list(value: &str) -> Self {
        value
            .split(',')
            .filter_map(|member| match member.trim() {
                "EVConnected" => Some(Self::EVConnected),
                "Authorized" => Some(Self::Authorized),
                "PowerPathClosed" => Some(Self::PowerPathClosed),
                _ => None,
            })
            .max()
            .unwrap_or_default()
    }
}

/// Where in a session OCPP says the transaction ends - `TxCtrlr.TxStopPoint`
/// (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV2.2).
///
/// **Stopping is not the mirror image of starting.** A start point is a condition that *begins* to
/// hold, and OCPP starts the transaction once every configured one does - so a configured set
/// resolves to its *latest* member (see [`TxStartPoint`]). A stop point is a condition that
/// *ceases* to hold, and a transaction cannot outlive the first of its conditions to lapse - so a
/// configured set resolves to its **earliest** member, which is why [`Self::from_member_list`]
/// takes the minimum where its start-point counterpart takes the maximum. The variants are
/// declared in the order they lapse along the end of a session, so `Ord` is that order.
///
/// Each point is one transition of this crate's own state machine, because that is the only place
/// where "the condition stopped holding" is observable here: the stop path is driven by an explicit
/// [`ConnectorEvent::ChargingStopped`] from the hardware binding rather than by polling each
/// condition. The three below are exactly the three [`TxStartPoint`] supports, and the declared
/// `values_list` is narrowed to them for the same reason - see [`TxStartPoint`]'s docs.
///
/// One combination OCPP itself warns about is reachable here: a stop point the charge point can
/// never observe lapsing leaves the transaction open forever. `EVConnected` on a station with
/// `OCPPCommCtrlr.UnlockOnEVSideDisconnect = false` is that case, because the cable is deliberately
/// never released (see [`ConnectorState::StoppingLocked`]). OCPP places the responsibility for
/// sensible start/stop combinations on the CSMS, and this crate does not second-guess a
/// configuration it was told to honour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub(crate) enum TxStopPoint {
    /// The authorization that permitted the session ended - the driver stopped it, the CSMS did,
    /// or the identifier was revoked. The earliest of the three, and the one an operator billing
    /// for time of use wants.
    Authorized,
    /// The contactor confirmed open, so energy can no longer flow. This crate's default, and what
    /// it did before `TxStopPoint` was honoured at all.
    #[default]
    PowerPathClosed,
    /// The cable left the connector, so the bay is free again - the transaction covers the whole
    /// time the connector was occupied, including the settling after energy stopped.
    EVConnected,
}

impl TxStopPoint {
    /// Parses `TxCtrlr.TxStopPoint`'s `MemberList` value, taking the *earliest* point named - see
    /// this type's docs for why the earliest and not the latest.
    ///
    /// An empty or unrecognised value falls back to the default, for the reason
    /// [`TxStartPoint::from_member_list`] gives: a charge point is better off ending transactions
    /// the conventional way than never ending them.
    pub fn from_member_list(value: &str) -> Self {
        value
            .split(',')
            .filter_map(|member| match member.trim() {
                "Authorized" => Some(Self::Authorized),
                "PowerPathClosed" => Some(Self::PowerPathClosed),
                "EVConnected" => Some(Self::EVConnected),
                _ => None,
            })
            .min()
            .unwrap_or_default()
    }
}

/// The configurable policy decisions a connector transition depends on
/// (`docs/OCPP-2.1-COMPLIANCE-ROADMAP.md` CV2.4).
///
/// [`ConnectorState::apply`] is a pure function of `(state, event, policy)` on purpose: the rules
/// stay testable without a device model, and the *reading* of the device model happens once, in
/// [`crate::state::ChargePointState::apply_connector_event`]. A CSMS changing one of these takes
/// effect on the next event without a reboot, like every other live variable in this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConnectorPolicy {
    /// `TxCtrlr.StopTxOnEVSideDisconnect` - **E09 vs E10**, the branch this type exists for.
    ///
    /// `true` (OCPP's default, and the safe one): the cable leaving the EV ends the transaction.
    /// `false`: the transaction is *suspended* instead and can resume if the cable comes back,
    /// which is what a driver who briefly unplugs to reseat a connector expects.
    pub stop_tx_on_ev_side_disconnect: bool,
    /// `TxCtrlr.TxStartPoint` - where in the session the transaction begins (CV2.2).
    pub tx_start_point: TxStartPoint,
    /// `TxCtrlr.TxStopPoint` - where in the session the transaction ends (CV2.2).
    pub tx_stop_point: TxStopPoint,
    /// `OCPPCommCtrlr.UnlockOnEVSideDisconnect` - **E09.FR.02 vs E09.FR.03**.
    ///
    /// `true` (OCPP's default): the cable leaving the EV releases this station's end too, so the
    /// driver can take the cable away. `false`: the station keeps hold of it until the driver
    /// authorizes again, which is how an operator stops a cable being walked off with. Only
    /// consulted when [`Self::stop_tx_on_ev_side_disconnect`] is on - E10's suspend path leaves the
    /// connector locked anyway (E10.FR.01), and OCPP says the other combination's behaviour is
    /// undefined.
    pub unlock_on_ev_side_disconnect: bool,
    /// `TxCtrlr.StopTxOnInvalidId` - whether a mid-session deauthorization stops the transaction
    /// at once (E05, CV2.5).
    pub stop_tx_on_invalid_id: bool,
    /// `TxCtrlr.MaxEnergyOnInvalidId`, in Wh - the last allowance granted when
    /// `stop_tx_on_invalid_id` is off. `None`/`0` means no allowance, which stops immediately.
    pub max_energy_on_invalid_id_wh: Option<i64>,
    /// `AuthCtrlr.AuthorizeRemoteStart` - **F01.FR.01 vs F01.FR.02** (CV7).
    ///
    /// `false` (this crate's default): a `RequestStartTransaction` starts the transaction
    /// immediately, and the CSMS checks the identifier when it processes the resulting
    /// `TransactionEvent` - F01.FR.02's path, and one fewer round trip.
    /// `true`: the request is authorized first, exactly as a card presented at the reader is, and
    /// energy transfer waits for that answer - F01.FR.01.
    ///
    /// Never consulted for an identifier whose kind is exempt; see
    /// [`IdTokenKind::requires_authorization`](crate::state::IdTokenKind::requires_authorization).
    pub authorize_remote_start: bool,
}

impl Default for ConnectorPolicy {
    /// OCPP's own defaults, so a charge point whose binding removed the variables behaves the way
    /// the spec says an unconfigured one should.
    fn default() -> Self {
        Self {
            stop_tx_on_ev_side_disconnect: true,
            tx_start_point: TxStartPoint::Authorized,
            tx_stop_point: TxStopPoint::PowerPathClosed,
            unlock_on_ev_side_disconnect: true,
            // OCPP's default, and the safe one: an identifier the CSMS has just refused should
            // not keep drawing energy nobody will be billed for.
            stop_tx_on_invalid_id: true,
            max_energy_on_invalid_id_wh: None,
            // F01.FR.02, and what this crate did before the variable was honoured at all: the
            // CSMS's own request stands as the decision, and it re-checks the identifier when the
            // `TransactionEvent` reaches it.
            authorize_remote_start: false,
        }
    }
}

impl ConnectorState {
    /// The status reported to the CSMS via StatusNotification for this connector state.
    ///
    /// Several internal states map to the same OCPP-visible status (e.g. `Locked` and
    /// `Charging` are both `Occupied`) - only an actual change in this mapped value should
    /// trigger a StatusNotification, not every internal transition.
    pub fn availability_status(&self) -> ConnectorStatus {
        match self {
            Self::Available => ConnectorStatus::Available,
            Self::Connected
            | Self::Locked
            | Self::Authorizing
            | Self::Starting
            | Self::Charging
            | Self::SuspendedEv
            | Self::SuspendedEvse
            | Self::Stopping
            | Self::StoppingLocked
            | Self::Finishing
            | Self::Unlocking => ConnectorStatus::Occupied,
            Self::Unavailable => ConnectorStatus::Unavailable,
            Self::Faulted | Self::FaultedSafe => ConnectorStatus::Faulted,
            Self::Reserved => ConnectorStatus::Reserved,
        }
    }

    pub(crate) fn apply(
        &mut self,
        event: ConnectorEvent,
        policy: ConnectorPolicy,
    ) -> ConnectorTransition {
        let (next, command) = match (*self, event) {
            // E05 (CV2.5): the identifier was revoked mid-session and the operator wants energy
            // cut at once. Same fail-safe stop as any other - contactor first.
            (
                Self::Charging | Self::SuspendedEv | Self::SuspendedEvse,
                ConnectorEvent::AuthorizationRevoked,
            ) if policy.stop_tx_on_invalid_id
                || policy.max_energy_on_invalid_id_wh.unwrap_or(0) <= 0 =>
            {
                (Self::Stopping, Some(ConnectorCommand::OpenContactor))
            }
            // E09.FR.03 (CV2.4): the same stop, on a station that keeps hold of its own end of the
            // cable. Checked before the ordinary E09 arm because it is the narrower rule - the
            // stop is identical, only its ending differs (see `Self::StoppingLocked`).
            (
                Self::Charging | Self::SuspendedEv | Self::SuspendedEvse,
                ConnectorEvent::CableDisconnected,
            ) if policy.stop_tx_on_ev_side_disconnect && !policy.unlock_on_ev_side_disconnect => {
                (Self::StoppingLocked, Some(ConnectorCommand::OpenContactor))
            }
            // E09/E10 (CV2.4): the cable left the EV while energy was flowing. Which of the two
            // use cases this is, is the operator's configuration - not something the charge point
            // gets to decide, and not something it may silently pick one of.
            //
            // Stopping opens the contactor first, exactly as any other stop does: the fail-safe
            // ordering is not negotiable just because the cable is already out on the EV side.
            (
                Self::Charging | Self::SuspendedEv | Self::SuspendedEvse,
                ConnectorEvent::CableDisconnected,
            ) if policy.stop_tx_on_ev_side_disconnect => {
                (Self::Stopping, Some(ConnectorCommand::OpenContactor))
            }
            // E10: suspended, not stopped. The transaction stays open and resumes if the cable
            // comes back - `ChargingResumed` from `SuspendedEv` is already the way back.
            (Self::Charging | Self::SuspendedEvse, ConnectorEvent::CableDisconnected) => {
                (Self::SuspendedEv, None)
            }
            (Self::Available | Self::Reserved, ConnectorEvent::CableConnected) => {
                (Self::Connected, Some(ConnectorCommand::Lock))
            }
            (Self::Available, ConnectorEvent::Reserved(_)) => (Self::Reserved, None),
            (Self::Reserved, ConnectorEvent::ReservationCancelled) => (Self::Available, None),
            // Expiry frees the connector exactly like a cancellation does; the two differ only in
            // what the CSMS is told afterwards - see `ReservationEndReason`.
            (Self::Reserved, ConnectorEvent::ReservationExpired) => (Self::Available, None),
            (Self::Connected, ConnectorEvent::LockConfirmed) => (Self::Locked, None),
            // A presentation landing before `LockConfirmed` - the hardware reported the cable
            // and the token in the same batch - must not be dropped: `Connected` is as much a
            // cable-in rest state as `Locked` is, and on a lockless board it is the *only*
            // latched state there is.
            (
                Self::Connected | Self::Locked,
                ConnectorEvent::IdTokenPresented(_),
            ) => (Self::Authorizing, None),
            // Plug & Charge takes the identical path: the certificate changes who can answer the
            // authorization question, not what the connector is waiting for while they do.
            (Self::Locked, ConnectorEvent::ContractCertificatePresented { .. }) => {
                (Self::Authorizing, None)
            }
            (Self::Locked, ConnectorEvent::RemoteUnlockRequested) => {
                (Self::Unlocking, Some(ConnectorCommand::Unlock))
            }
            // F01.FR.01 (CV7): the operator asked for a remote start to be authorized like a local
            // one, so the connector waits for a decision instead of closing the contactor. The
            // path from here is byte-for-byte the one a presented card takes - `Authorizing`, then
            // `ChargingAuthorized`/`AuthorizationDenied` - which is what the requirement means by
            // "behave as if in response to a local action".
            //
            // Checked before the immediate-start arm because it is the narrower rule.
            (Self::Locked, ConnectorEvent::RemoteStartRequested { ref id_token, .. })
                if policy.authorize_remote_start && id_token.kind.requires_authorization() =>
            {
                (Self::Authorizing, None)
            }
            (Self::Locked, ConnectorEvent::RemoteStartRequested { .. }) => {
                (Self::Starting, Some(ConnectorCommand::CloseContactor))
            }
            // F01 is "the cable is already in" - which `Connected` (lock confirmation still in
            // flight, or a lockless board) and `Finishing` (session over, cable still in)
            // satisfy exactly as `Locked` does. Same two-arm split: authorize first when
            // F01.FR.01 says so.
            (
                Self::Connected | Self::Finishing,
                ConnectorEvent::RemoteStartRequested { ref id_token, .. },
            ) if policy.authorize_remote_start && id_token.kind.requires_authorization() =>
            {
                (Self::Authorizing, None)
            }
            (
                Self::Connected | Self::Finishing,
                ConnectorEvent::RemoteStartRequested { .. },
            ) => (Self::Starting, Some(ConnectorCommand::CloseContactor)),
            (Self::Authorizing, ConnectorEvent::ChargingAuthorized(_)) => {
                (Self::Starting, Some(ConnectorCommand::CloseContactor))
            }
            (Self::Authorizing, ConnectorEvent::AuthorizationDenied) => (Self::Locked, None),
            (Self::Starting, ConnectorEvent::ContactorClosed) => (Self::Charging, None),
            // Suspension is a pause *within* a running transaction, not a stop: the contactor
            // stays closed and the cable stays locked, so the way out is either a resume or the
            // same `ChargingStopped` path any charging connector takes.
            (Self::Charging | Self::SuspendedEvse, ConnectorEvent::ChargingSuspendedByEv) => {
                (Self::SuspendedEv, None)
            }
            (Self::Charging | Self::SuspendedEv, ConnectorEvent::ChargingSuspendedByEvse) => {
                (Self::SuspendedEvse, None)
            }
            (Self::SuspendedEv | Self::SuspendedEvse, ConnectorEvent::ChargingResumed) => {
                (Self::Charging, None)
            }
            (
                Self::Charging | Self::SuspendedEv | Self::SuspendedEvse,
                ConnectorEvent::ChargingStopped(_),
            ) => (Self::Stopping, Some(ConnectorCommand::OpenContactor)),
            // A CSMS-initiated `Reset` (Immediate) interrupts any state where a cable is
            // engaged, reusing the exact same fail-safe stop (open contactor, then unlock via
            // `Stopping`/`Finishing`) as a normal charging stop rather than a parallel path
            // (e.g. `Faulted`/`FaultedSafe`, which would misreport this connector as faulted to
            // the CSMS). Already-idle states (`Available`/`Reserved`/`Unavailable`/faulted) and
            // already-settling ones (`Stopping`/`Finishing`/`Unlocking`) are unaffected - see
            // `docs/ROADMAP.md` §2.
            (
                Self::Connected
                | Self::Locked
                | Self::Authorizing
                | Self::Starting
                | Self::Charging
                | Self::SuspendedEv
                | Self::SuspendedEvse,
                ConnectorEvent::ResetRequested,
            ) => (Self::Stopping, Some(ConnectorCommand::OpenContactor)),
            (Self::Stopping, ConnectorEvent::ContactorOpened) => {
                (Self::Finishing, Some(ConnectorCommand::Unlock))
            }
            // E09.FR.03: contactor confirmed open, and no unlock. The cable is still latched with
            // no transaction running, which is exactly `Locked` - so the driver's next identifier
            // (or a CSMS `UnlockConnector`) is what releases it, as the requirement asks.
            (Self::StoppingLocked, ConnectorEvent::ContactorOpened) => (Self::Locked, None),
            // `Finishing` is the post-transaction rest state while the cable stays in. The
            // unlock confirmation completing the stop sequence must not end it - only the
            // cable leaving does, which is what frees the bay (and, under
            // `TxStopPoint::EVConnected`, finally closes the transaction).
            (Self::Finishing, ConnectorEvent::UnlockConfirmed) => (Self::Finishing, None),
            (Self::Finishing, ConnectorEvent::CableDisconnected) => (Self::Available, None),
            // The hardware only reports edges, so a missed unplug/replug pair surfaces as
            // `CableConnected` while already `Finishing` - recover by re-locking through
            // `Connected` rather than wedging.
            (Self::Finishing, ConnectorEvent::CableConnected) => {
                (Self::Connected, Some(ConnectorCommand::Lock))
            }
            // A post-session re-swipe is a fresh presentation - the same `Authorizing` wait a
            // token gets anywhere else the cable is in.
            (Self::Finishing, ConnectorEvent::IdTokenPresented(_)) => (Self::Authorizing, None),
            (Self::Connected, ConnectorEvent::CableDisconnected) => (Self::Available, None),
            (_, ConnectorEvent::SetUnavailable) => (Self::Unavailable, None),
            (Self::Faulted | Self::FaultedSafe, ConnectorEvent::SetAvailable) => (*self, None),
            (_, ConnectorEvent::SetAvailable) => (Self::Available, None),
            (Self::Faulted, ConnectorEvent::ContactorOpened) => (Self::FaultedSafe, None),
            (Self::FaultedSafe, ConnectorEvent::FaultCleared) => {
                (Self::Unlocking, Some(ConnectorCommand::Unlock))
            }
            (Self::Unlocking, ConnectorEvent::UnlockConfirmed) => (Self::Available, None),
            // G05.FR.01 (CV11): a retention lock that will not engage must not be charged
            // through, so a lock failure takes the identical fail-safe path a fault does. What
            // makes it distinguishable to the CSMS is the `NotifyEvent` and the
            // `ConnectorPlugRetentionLock`/`Problem` variable that go with it, not a different
            // connector state - there is no safer state than the one a fault already produces.
            (_, ConnectorEvent::FaultDetected | ConnectorEvent::LockFailed) => {
                (Self::Faulted, Some(ConnectorCommand::OpenContactor))
            }
            (state, _) => (state, None),
        };

        let changed = *self != next;
        *self = next;
        ConnectorTransition { changed, command }
    }
}

#[cfg(test)]
mod suspension_tests {
    use super::*;
    use crate::state::{ConnectorStatus, StopReason};

    fn charging() -> ConnectorState {
        ConnectorState::Charging
    }

    #[test]
    fn either_side_can_suspend_charging_and_resume_it() {
        let mut connector = charging();

        let transition = connector.apply(
            ConnectorEvent::ChargingSuspendedByEv,
            ConnectorPolicy::default(),
        );
        assert!(transition.changed);
        assert_eq!(connector, ConnectorState::SuspendedEv);
        // Suspension is a pause, not a stop: nothing is asked of the contactor.
        assert!(transition.command.is_none());

        assert!(
            connector
                .apply(ConnectorEvent::ChargingResumed, ConnectorPolicy::default())
                .changed
        );
        assert_eq!(connector, ConnectorState::Charging);

        assert!(
            connector
                .apply(
                    ConnectorEvent::ChargingSuspendedByEvse,
                    ConnectorPolicy::default()
                )
                .changed
        );
        assert_eq!(connector, ConnectorState::SuspendedEvse);
        assert!(
            connector
                .apply(ConnectorEvent::ChargingResumed, ConnectorPolicy::default())
                .changed
        );
        assert_eq!(connector, ConnectorState::Charging);
    }

    #[test]
    fn a_suspension_can_change_sides_without_passing_through_charging() {
        // The EV stops drawing, then the EVSE cuts supply too (or the reverse) - a real sequence,
        // and reporting a spurious "charging" in between would be wrong.
        let mut connector = charging();
        connector.apply(
            ConnectorEvent::ChargingSuspendedByEv,
            ConnectorPolicy::default(),
        );

        assert!(
            connector
                .apply(
                    ConnectorEvent::ChargingSuspendedByEvse,
                    ConnectorPolicy::default()
                )
                .changed
        );
        assert_eq!(connector, ConnectorState::SuspendedEvse);

        assert!(
            connector
                .apply(
                    ConnectorEvent::ChargingSuspendedByEv,
                    ConnectorPolicy::default()
                )
                .changed
        );
        assert_eq!(connector, ConnectorState::SuspendedEv);
    }

    #[test]
    fn resuming_a_connector_that_is_not_suspended_does_nothing() {
        let mut connector = ConnectorState::Locked;
        let transition =
            connector.apply(ConnectorEvent::ChargingResumed, ConnectorPolicy::default());
        assert!(!transition.changed);
        assert_eq!(connector, ConnectorState::Locked);
    }

    #[test]
    fn a_suspended_connector_stops_through_the_same_path_a_charging_one_does() {
        for suspended in [
            ConnectorEvent::ChargingSuspendedByEv,
            ConnectorEvent::ChargingSuspendedByEvse,
        ] {
            let mut connector = charging();
            connector.apply(suspended, ConnectorPolicy::default());

            let transition = connector.apply(
                ConnectorEvent::ChargingStopped(StopReason::Local),
                ConnectorPolicy::default(),
            );

            assert_eq!(connector, ConnectorState::Stopping);
            assert!(matches!(
                transition.command,
                Some(ConnectorCommand::OpenContactor)
            ));
        }
    }

    #[test]
    fn a_suspended_connector_still_faults_and_still_resets_fail_safe() {
        let mut connector = charging();
        connector.apply(
            ConnectorEvent::ChargingSuspendedByEv,
            ConnectorPolicy::default(),
        );
        let transition = connector.apply(ConnectorEvent::FaultDetected, ConnectorPolicy::default());
        assert_eq!(connector, ConnectorState::Faulted);
        assert!(matches!(
            transition.command,
            Some(ConnectorCommand::OpenContactor)
        ));

        let mut connector = charging();
        connector.apply(
            ConnectorEvent::ChargingSuspendedByEvse,
            ConnectorPolicy::default(),
        );
        let transition =
            connector.apply(ConnectorEvent::ResetRequested, ConnectorPolicy::default());
        assert_eq!(connector, ConnectorState::Stopping);
        assert!(matches!(
            transition.command,
            Some(ConnectorCommand::OpenContactor)
        ));
    }

    #[test]
    fn a_suspended_connector_is_still_occupied_to_2_x() {
        // 2.x's connector status has no suspended value at all - it moved the distinction onto
        // the transaction's chargingState - so both suspended states must stay `Occupied` here.
        assert_eq!(
            ConnectorState::SuspendedEv.availability_status(),
            ConnectorStatus::Occupied
        );
        assert_eq!(
            ConnectorState::SuspendedEvse.availability_status(),
            ConnectorStatus::Occupied
        );
    }

    #[test]
    fn charging_cannot_be_suspended_before_it_starts() {
        for state in [
            ConnectorState::Connected,
            ConnectorState::Locked,
            ConnectorState::Authorizing,
            ConnectorState::Starting,
            ConnectorState::Finishing,
        ] {
            let mut connector = state;
            assert!(
                !connector
                    .apply(
                        ConnectorEvent::ChargingSuspendedByEv,
                        ConnectorPolicy::default()
                    )
                    .changed
            );
            assert_eq!(connector, state);
        }
    }
}

#[cfg(test)]
mod finishing_tests {
    use super::*;
    use crate::state::{IdToken, IdTokenKind, StopReason};

    /// Drives a connector through a real stop so tests start from the state the machine
    /// actually settles into - not a constructed assumption of one.
    fn finishing() -> ConnectorState {
        let mut connector = ConnectorState::Charging;
        connector.apply(
            ConnectorEvent::ChargingStopped(StopReason::Remote),
            ConnectorPolicy::default(),
        );
        connector.apply(ConnectorEvent::ContactorOpened, ConnectorPolicy::default());
        assert_eq!(connector, ConnectorState::Finishing);
        connector
    }

    fn id_token() -> IdToken {
        IdToken {
            value: "04A224B2".into(),
            kind: IdTokenKind::ISO14443,
        }
    }

    /// `Finishing` is the post-transaction rest state while the cable stays in - which is
    /// exactly what 1.6J's `Finishing` wire status means and what an operator billing for bay
    /// occupancy needs to see. The unlock confirmation completing the stop sequence must not
    /// end it; only the cable leaving does.
    #[test]
    fn finishing_holds_until_the_cable_leaves() {
        let mut connector = finishing();

        assert!(
            !connector
                .apply(
                    ConnectorEvent::UnlockConfirmed,
                    ConnectorPolicy::default()
                )
                .changed
        );
        assert_eq!(connector, ConnectorState::Finishing);

        assert!(
            connector
                .apply(
                    ConnectorEvent::CableDisconnected,
                    ConnectorPolicy::default()
                )
                .changed
        );
        assert_eq!(connector, ConnectorState::Available);
    }

    /// The cable never left, so the bay is still occupied - and OCPP F01's premise (start now,
    /// the cable is already in) holds on `Finishing` exactly as on `Locked`.
    #[test]
    fn finishing_accepts_a_remote_start() {
        let mut connector = finishing();

        let transition = connector.apply(
            ConnectorEvent::RemoteStartRequested {
                id_token: id_token(),
                remote_start_id: Some(7),
            },
            ConnectorPolicy::default(),
        );

        assert_eq!(connector, ConnectorState::Starting);
        assert!(matches!(
            transition.command,
            Some(ConnectorCommand::CloseContactor)
        ));
    }

    /// F01.FR.01, on `Finishing`: the operator asked for remote starts to be authorized like
    /// local ones, so the connector waits for the decision rather than closing the contactor.
    #[test]
    fn a_remote_start_from_finishing_authorizes_first_when_policy_says_so() {
        let mut connector = finishing();
        let policy = ConnectorPolicy {
            authorize_remote_start: true,
            ..Default::default()
        };

        connector.apply(
            ConnectorEvent::RemoteStartRequested {
                id_token: id_token(),
                remote_start_id: None,
            },
            policy,
        );

        assert_eq!(connector, ConnectorState::Authorizing);
    }

    /// `Connected` covers both a connector whose `LockConfirmed` has not landed yet and a
    /// lockless board resting post-plug - F01 must not wait on a lock that may not exist.
    #[test]
    fn connected_accepts_a_remote_start() {
        let mut connector = ConnectorState::Connected;

        let transition = connector.apply(
            ConnectorEvent::RemoteStartRequested {
                id_token: id_token(),
                remote_start_id: None,
            },
            ConnectorPolicy::default(),
        );

        assert_eq!(connector, ConnectorState::Starting);
        assert!(matches!(
            transition.command,
            Some(ConnectorCommand::CloseContactor)
        ));
    }

    #[test]
    fn a_remote_start_from_connected_authorizes_first_when_policy_says_so() {
        let mut connector = ConnectorState::Connected;
        let policy = ConnectorPolicy {
            authorize_remote_start: true,
            ..Default::default()
        };

        connector.apply(
            ConnectorEvent::RemoteStartRequested {
                id_token: id_token(),
                remote_start_id: None,
            },
            policy,
        );

        assert_eq!(connector, ConnectorState::Authorizing);
    }

    /// A presented token starts authorization from any cable-in rest state - `Connected` is
    /// where it lands when the cable event and the presentation arrive in the same batch (the
    /// token reaches the machine before `LockConfirmed` does), and `Finishing` is a re-swipe
    /// after the previous session.
    #[test]
    fn a_token_presented_on_connected_or_finishing_authorizes() {
        for state in [ConnectorState::Connected, ConnectorState::Finishing] {
            let mut connector = state;

            connector.apply(
                ConnectorEvent::IdTokenPresented(id_token()),
                ConnectorPolicy::default(),
            );

            assert_eq!(connector, ConnectorState::Authorizing, "from {state:?}");
        }
    }

    /// The hardware only reports edges, so a missed unplug/replug pair surfaces as
    /// `CableConnected` while already `Finishing` - recover by re-locking through `Connected`
    /// rather than wedging.
    #[test]
    fn a_cable_seen_still_in_finishing_relocks_the_connector() {
        let mut connector = finishing();

        let transition = connector.apply(
            ConnectorEvent::CableConnected,
            ConnectorPolicy::default(),
        );

        assert!(transition.changed);
        assert_eq!(connector, ConnectorState::Connected);
        assert!(matches!(transition.command, Some(ConnectorCommand::Lock)));
    }
}
