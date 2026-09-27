use std::net::UdpSocket;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hickory_proto::rr::RData;

use super::{ControlError, ServiceControl};
use crate::dns::{Cancellation, Deadline, RuntimeRevision};
use crate::runtime::{
    PreparedRuntime, RuntimeCoordinator, SystemClock, SystemSocketFactory, bind_prepared,
    bind_prepared_reusing,
};
use crate::service::tests::{runtime_config_with_answer_at, udp_query};
use crate::service::{DnsService, ServiceReloadError};

fn deadline() -> Deadline {
    Deadline::new(Instant::now() + Duration::from_secs(5))
}

fn prepared(port: u16, revision: u64) -> PreparedRuntime {
    let work = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("_fluxdns/p1-service-control")
        .to_string_lossy()
        .replace('\\', "/");
    PreparedRuntime::prepare_with_policy_core(
        runtime_config_with_answer_at(&work, port, &format!("127.0.0.{revision}")),
        RuntimeRevision(revision),
    )
    .unwrap()
}

async fn service() -> (DnsService, ServiceControl, u16) {
    let port = UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let bound = bind_prepared(
        prepared(port, 1),
        &SystemSocketFactory::new(),
        deadline(),
        &Cancellation::new(),
    )
    .await
    .unwrap();
    let service =
        DnsService::with_default_timeout_from_coordinator(Arc::new(RuntimeCoordinator::new(bound)))
            .unwrap();
    let control = service.control();
    (service, control, port)
}

async fn assert_answer(port: u16, revision: u8) {
    let response = udp_query(([127, 0, 0, 1], port).into(), 91, "example.test.").await;
    assert!(
        response.answers.iter().any(|record| matches!(
            &record.data,
            RData::A(address) if address.0.octets() == [127, 0, 0, revision]
        )),
        "unexpected DNS response: {response:?}"
    );
}

#[tokio::test]
async fn control_rejects_full_expired_invalid_and_closed_commands() {
    let (mut service, control, port) = service().await;
    assert!(matches!(
        control.try_apply(RuntimeRevision(1), prepared(port, 3), deadline()),
        Err(ControlError::InvalidCandidateRevision)
    ));
    assert!(matches!(
        control.try_apply(
            RuntimeRevision(1),
            prepared(port, 2),
            Deadline::new(Instant::now())
        ),
        Err(ControlError::Expired)
    ));
    let first = control
        .try_apply(RuntimeRevision(1), prepared(port, 2), deadline())
        .unwrap();
    assert!(matches!(
        control.try_apply(RuntimeRevision(1), prepared(port, 2), deadline()),
        Err(ControlError::Busy)
    ));
    assert_answer(port, 1).await;
    service
        .shutdown(&SystemClock::new(), deadline())
        .await
        .unwrap();
    assert!(matches!(
        first.outcome().await,
        Err(ControlError::Unavailable)
    ));
    assert!(matches!(
        control.try_apply(RuntimeRevision(1), prepared(port, 2), deadline()),
        Err(ControlError::Unavailable)
    ));
}

#[tokio::test]
async fn control_owner_loop_applies_after_receipt_is_dropped_and_rejects_stale_revision() {
    let (mut service, control, port) = service().await;
    let coordinator = Arc::clone(service.coordinator());
    drop(
        control
            .try_apply(RuntimeRevision(1), prepared(port, 2), deadline())
            .unwrap(),
    );
    let (stop, signal) = tokio::sync::oneshot::channel();
    let owner = service.run_with_reload(
        Duration::from_secs(2),
        Duration::from_secs(60),
        |_| Box::pin(async { Ok(()) }),
        async {
            signal.await.unwrap();
            Ok(())
        },
    );
    let caller = async {
        tokio::time::timeout(Duration::from_secs(2), async {
            while coordinator.current_revision() != RuntimeRevision(2) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_answer(port, 2).await;
        let stale = control
            .try_apply(RuntimeRevision(1), prepared(port, 2), deadline())
            .unwrap();
        assert!(matches!(
            stale.outcome().await,
            Err(ControlError::RevisionConflict {
                expected: RuntimeRevision(1),
                actual: RuntimeRevision(2)
            })
        ));
        let next = control
            .try_apply(RuntimeRevision(2), prepared(port, 3), deadline())
            .unwrap();
        assert_eq!(next.outcome().await.unwrap(), RuntimeRevision(3));
        assert_answer(port, 3).await;
        stop.send(()).unwrap();
    };
    let (result, ()) = tokio::join!(owner, caller);
    assert!(!result.unwrap().deadline_expired);
}

#[tokio::test]
async fn control_bind_failure_keeps_the_previous_runtime_serving() {
    let (mut service, control, port) = service().await;
    let occupied = UdpSocket::bind("127.0.0.1:0").unwrap();
    let receipt = control
        .try_apply(
            RuntimeRevision(1),
            prepared(occupied.local_addr().unwrap().port(), 2),
            deadline(),
        )
        .unwrap();
    let command = service.control_commands.recv().await.unwrap();
    service.apply_control_command(command).await;
    assert!(matches!(
        receipt.outcome().await,
        Err(ControlError::Apply(ServiceReloadError::Bind(_)))
    ));
    assert_eq!(service.runtime().revision(), RuntimeRevision(1));
    assert!(!service.runtime().is_draining());
    assert_answer(port, 1).await;
    service
        .shutdown(&SystemClock::new(), deadline())
        .await
        .unwrap();
}

#[tokio::test]
async fn control_expiration_while_queued_does_not_apply_or_replay() {
    let (mut service, control, port) = service().await;
    let receipt = control
        .try_apply(
            RuntimeRevision(1),
            prepared(port, 2),
            Deadline::new(Instant::now() + Duration::from_millis(20)),
        )
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    let command = service.control_commands.recv().await.unwrap();
    service.apply_control_command(command).await;
    assert!(matches!(
        receipt.result.await.unwrap(),
        Err(ControlError::Expired)
    ));
    assert_eq!(service.runtime().revision(), RuntimeRevision(1));
    assert_answer(port, 1).await;
    service
        .shutdown(&SystemClock::new(), deadline())
        .await
        .unwrap();
}

#[tokio::test]
async fn control_lost_owner_and_response_timeout_are_unknown_not_rejected() {
    let (mut service, control, port) = service().await;
    let lost = control
        .try_apply(RuntimeRevision(1), prepared(port, 2), deadline())
        .unwrap();
    drop(service.control_commands.recv().await.unwrap());
    assert!(matches!(
        lost.outcome().await,
        Err(ControlError::OutcomeUnknown)
    ));
    let timed_out = control
        .try_apply(
            RuntimeRevision(1),
            prepared(port, 2),
            Deadline::new(Instant::now() + Duration::from_millis(10)),
        )
        .unwrap();
    assert!(matches!(
        timed_out.outcome().await,
        Err(ControlError::OutcomeUnknown)
    ));
    assert_eq!(service.runtime().revision(), RuntimeRevision(1));
    service
        .shutdown(&SystemClock::new(), deadline())
        .await
        .unwrap();
}

#[tokio::test]
async fn control_owner_can_stop_while_waiting_for_the_runtime_mutation_gate() {
    let (mut service, control, port) = service().await;
    let coordinator = Arc::clone(service.coordinator());
    let current = coordinator.load();
    let bound = bind_prepared_reusing(
        prepared(port, 2),
        current.listeners(),
        &SystemSocketFactory::new(),
        deadline(),
        &Cancellation::new(),
    )
    .await
    .unwrap();
    let activation = coordinator
        .prepare_service_activation(RuntimeRevision(1), bound)
        .await
        .unwrap();
    let receipt = control
        .try_apply(RuntimeRevision(1), prepared(port, 2), deadline())
        .unwrap();
    let (stop, signal) = tokio::sync::oneshot::channel();
    let owner = service.run_with_reload(
        Duration::from_secs(2),
        Duration::from_secs(60),
        |_| Box::pin(async { Ok(()) }),
        async {
            signal.await.unwrap();
            Ok(())
        },
    );
    let caller = async {
        tokio::time::timeout(Duration::from_secs(1), async {
            while control.sender.capacity() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_answer(port, 1).await;
        stop.send(()).unwrap();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(owner, caller)
    })
    .await
    .unwrap();
    assert!(!result.unwrap().deadline_expired);
    assert!(matches!(
        receipt.outcome().await,
        Err(ControlError::OutcomeUnknown)
    ));
    assert_eq!(coordinator.current_revision(), RuntimeRevision(1));
    drop(activation);
}
