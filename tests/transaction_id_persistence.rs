//! The OCPP 1.6J CSMS-assigned transaction id, made durable.
//!
//! 1.6J is the only version where the *CSMS* mints the transaction id (`StartTransaction.conf`),
//! which makes the id cache inside `Ocpp1_6TransactionNotifier` the one piece of session state a
//! reboot loses that cannot be rebuilt locally. `with_id_store` persists each assigned id under
//! `ocpp-cp/txn-csms-id/<internal id>`; these tests drive a real notifier against the scripted
//! mock CSMS to pin what that buys:
//!
//! - a plain `Started` persists the id the CSMS handed back;
//! - a `Started` that finds its id already stored is a *resumed* session - the cache is seeded
//!   and no `StartTransaction` goes on the wire (a second call would mint a second CSMS-side
//!   transaction for one physical session);
//! - `Updated`/`Ended` fall back to the store when the RAM cache is empty (a mid-session
//!   reboot), and `Ended` removes the key once the session is closed.

mod common;

use std::sync::Arc;

use common::MockCsms;
use ocpp_charge_point::clock::SystemClock;
use ocpp_charge_point::hardware::{InMemoryStorage, Storage};
use ocpp_charge_point::state::{
    IdToken, IdTokenKind, MeterSample, StopReason, Transaction, TransactionChargingState,
    TransactionEventKind, TransactionId, TransactionUpdateReason,
};
use ocpp_charge_point::transactions::{Ocpp1_6TransactionNotifier, TransactionNotifier};
use serde_json::json;

/// The id the scripted CSMS assigns - arbitrary, but stable across the tests below.
const CSMS_ID: i64 = 4_242;

fn csms_id_key(internal_id: u64) -> String {
    format!("ocpp-cp/txn-csms-id/{internal_id}")
}

fn transaction(id: u64, energy_wh: Option<i64>, stop_reason: Option<StopReason>) -> Transaction {
    Transaction {
        id: TransactionId(id),
        id_token: Some(IdToken {
            value: "04A224B2".into(),
            kind: IdTokenKind::ISO14443,
        }),
        charging_state: TransactionChargingState::Charging,
        stop_reason,
        seq_no: 0,
        last_meter_sample: energy_wh.map(|energy_wh| MeterSample {
            energy_wh,
            ..Default::default()
        }),
        priority_charging: false,
        remote_start_id: None,
        reservation_id: None,
        stop_at_energy_wh: None,
        limit: None,
        csms_limit: None,
        limit_reached: None,
        energy_start_wh: None,
        elapsed_secs: None,
    }
}

/// A notifier wired to a live client and a durable id store, against a CSMS scripted to answer
/// `StartTransaction` with [`CSMS_ID`].
async fn notifier_with_store(
    storage: Arc<InMemoryStorage>,
) -> (
    MockCsms,
    Ocpp1_6TransactionNotifier<SystemClock, Arc<InMemoryStorage>>,
) {
    let csms = MockCsms::start(
        "ocpp1.6",
        vec![(
            "StartTransaction",
            json!({ "transactionId": CSMS_ID, "idTagInfo": { "status": "Accepted" } }),
        )],
    )
    .await;
    let client = ocpp_client::connect_1_6(csms.address(), None)
        .await
        .expect("the mock CSMS should accept the connection");
    let notifier =
        Ocpp1_6TransactionNotifier::with_clock(client, [1], SystemClock).with_id_store(storage);
    (csms, notifier)
}

#[tokio::test]
async fn a_start_persists_the_csms_assigned_transaction_id() {
    let storage = Arc::new(InMemoryStorage::new());
    let (csms, notifier) = notifier_with_store(storage.clone()).await;

    notifier
        .notify_transaction_event(
            0,
            0,
            TransactionEventKind::Started,
            transaction(7, Some(0), None),
            false,
        )
        .await
        .expect("StartTransaction should be acknowledged");

    let call = csms.wait_for("StartTransaction").await;
    assert_eq!(call.payload["connectorId"], 1);
    assert_eq!(
        storage
            .get(&csms_id_key(7))
            .await
            .expect("the store read")
            .as_deref(),
        Some(&CSMS_ID.to_le_bytes()[..]),
        "the CSMS-assigned id must reach durable storage"
    );
}

#[tokio::test]
async fn a_started_with_a_stored_id_resumes_without_a_wire_call() {
    let storage = Arc::new(InMemoryStorage::new());
    // The record a reboot left behind: transaction 7 was already acknowledged as CSMS id 4242.
    storage
        .set(&csms_id_key(7), &CSMS_ID.to_le_bytes())
        .await
        .unwrap();
    let (csms, notifier) = notifier_with_store(storage).await;

    notifier
        .notify_transaction_event(
            0,
            0,
            TransactionEventKind::Started,
            transaction(7, Some(1_200), None),
            false,
        )
        .await
        .expect("a resume should be acknowledged");

    // The seeded cache is what makes the resumed session reportable: the next update must carry
    // the stored CSMS id. Driving one also bounds the negative assertion below - a StartTransaction,
    // had one been sent, would necessarily precede this MeterValues on the wire.
    notifier
        .notify_transaction_event(
            0,
            0,
            TransactionEventKind::Updated(TransactionUpdateReason::MeterValuePeriodic),
            transaction(7, Some(1_300), None),
            false,
        )
        .await
        .expect("the resumed session should still report");
    let meter = csms.wait_for("MeterValues").await;
    assert_eq!(meter.payload["transactionId"], CSMS_ID);

    assert!(
        !csms
            .actions()
            .iter()
            .any(|action| action == "StartTransaction"),
        "a resumed session must not re-run StartTransaction: {:?}",
        csms.actions()
    );
}

#[tokio::test]
async fn an_update_falls_back_to_the_stored_id_on_a_cache_miss() {
    let storage = Arc::new(InMemoryStorage::new());
    storage
        .set(&csms_id_key(7), &CSMS_ID.to_le_bytes())
        .await
        .unwrap();
    // A fresh notifier: the RAM cache a reboot would have emptied.
    let (csms, notifier) = notifier_with_store(storage).await;

    notifier
        .notify_transaction_event(
            0,
            0,
            TransactionEventKind::Updated(TransactionUpdateReason::MeterValuePeriodic),
            transaction(7, Some(1_300), None),
            false,
        )
        .await
        .expect("the update should still report");

    let meter = csms.wait_for("MeterValues").await;
    assert_eq!(meter.payload["transactionId"], CSMS_ID);
}

#[tokio::test]
async fn an_end_reports_the_stored_id_and_removes_it() {
    let storage = Arc::new(InMemoryStorage::new());
    storage
        .set(&csms_id_key(7), &CSMS_ID.to_le_bytes())
        .await
        .unwrap();
    let (csms, notifier) = notifier_with_store(storage.clone()).await;

    notifier
        .notify_transaction_event(
            0,
            0,
            TransactionEventKind::Ended,
            transaction(7, Some(4_200), Some(StopReason::PowerLoss)),
            false,
        )
        .await
        .expect("the stop should be acknowledged");

    let stop = csms.wait_for("StopTransaction").await;
    assert_eq!(stop.payload["transactionId"], CSMS_ID);
    assert_eq!(stop.payload["reason"], "PowerLoss");
    assert_eq!(
        storage.get(&csms_id_key(7)).await.expect("the store read"),
        None,
        "the id's job is done once the session is closed - leaving it would read as a phantom resume"
    );
}
